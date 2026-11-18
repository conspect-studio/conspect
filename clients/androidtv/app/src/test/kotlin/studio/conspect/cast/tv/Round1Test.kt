package studio.conspect.cast.tv

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger


class FrameMonitorTest {
    @Test fun healthyStreamNeverStalls() {
        val m = FrameMonitor(2000)
        var t = 0L
        repeat(300) { m.onFrame(t); t += 33; assertFalse(m.stalled(t)) } // 10 s of frames at 30 fps
        assertTrue(m.frames == 300L)
    }

    @Test fun freezeStallsAfterTwoSeconds() {
        val m = FrameMonitor(2000)
        m.onFrame(1000)
        assertFalse(m.stalled(2999))
        assertTrue(m.stalled(3001))
    }

    @Test fun noFrameYetIsNotAStall() {
        assertFalse(FrameMonitor(2000).stalled(60_000)) // the 2.0 s first-frame deadline covers this case
    }
}

class IdrFilterTest {
    private val vpid = 0x100
    private val apid = 0x101
    private var cc = 0
    private var acc = 0

    private fun pkt(pid: Int, counter: Int, start: Boolean, payload: ByteArray): ByteArray {
        val p = ByteArray(188) { 0xff.toByte() }
        p[0] = 0x47
        p[1] = (((if (start) 0x40 else 0) or (pid shr 8))).toByte()
        p[2] = (pid and 0xff).toByte()
        p[3] = (0x10 or (counter and 0xf)).toByte()
        var o = 4
        if (start && pid == vpid) { p[4] = 0; p[5] = 0; p[6] = 1; p[7] = 0xe0.toByte(); o = 8 }
        System.arraycopy(payload, 0, p, o, minOf(payload.size, 188 - o))
        return p
    }

    private fun nal(type: Int) = byteArrayOf(0, 0, 0, 1, type.toByte(), 1, 2, 3)

    /** One access unit of [n] packets: first packet carries the NAL header of [type]. */
    private fun unit(type: Int, n: Int, skip: Int = -1): List<ByteArray> {
        val l = ArrayList<ByteArray>()
        for (i in 0 until n) {
            val c = cc; cc = (cc + 1) and 0xf
            if (i == skip) continue // lost on the wire
            l.add(pkt(vpid, c, i == 0, if (i == 0) nal(type) else byteArrayOf(9, 9, 9)))
        }
        return l
    }

    private fun run(f: TsIdrFilter, packets: List<ByteArray>): List<ByteArray> {
        val out = ArrayList<ByteArray>()
        for (p in packets) f.process(p, 0, 188).let { r -> r.toList().chunked(188).forEach { out.add(it.toByteArray()) } }
        return out
    }

    private fun c(l: List<ByteArray>) = l.map { it.toList() }

    private fun isUnitStart(p: ByteArray, type: Int) = (p[1].toInt() and 0x40) != 0 && (p[12].toInt() and 0x1f) == type

    /** A last one-packet unit releases the unit before it (a unit is released when the next one starts). */
    private fun tail() = unit(1, 1)

    @Test fun healthyStreamPassesUntouched() {
        val f = TsIdrFilter()
        val input = unit(5, 3) + unit(1, 3) + unit(1, 2)
        val out = run(f, input + tail())
        assertEquals(input.size, out.size) // everything but the held tail
        assertEquals(c(input), c(out))
        assertEquals(0, f.recoveries)
    }

    @Test fun droppedVideoPacketAfterKeyframeDiscardsToNextIdrWithoutReconnect() {
        val f = TsIdrFilter()
        val idr1 = unit(5, 3)
        val broken = unit(1, 4, skip = 2)
        val p1 = unit(1, 3)
        val p2 = unit(1, 3)
        val idr2 = unit(5, 3)
        val p3 = unit(1, 2)
        val out = run(f, idr1 + broken + p1 + p2 + idr2 + p3 + tail())
        assertEquals(1, f.recoveries)
        // Out: the first IDR, then the second IDR and the P unit after it. The broken unit (a partial P frame) and
        // the two P frames that depend on it never reach the extractor; the connection is never reopened.
        assertEquals(c(idr1 + idr2 + p3), c(out))
        assertFalse(f.recovering)
    }

    @Test fun gapInsideAnIdrDropsItAndWaitsForTheNext() {
        val f = TsIdrFilter()
        val out = run(f, unit(5, 3) + unit(5, 3, skip = 1) + unit(1, 2) + unit(5, 2) + unit(1, 2) + tail())
        assertEquals(2, out.count { isUnitStart(it, 5) } + 0 * out.size) // the first IDR and the last one
        assertEquals(1, f.recoveries)
    }

    @Test fun audioAlwaysPasses() {
        val f = TsIdrFilter()
        val input = ArrayList<ByteArray>()
        input += unit(5, 2)
        input += unit(1, 3, skip = 1)
        input += pkt(apid, 0, true, byteArrayOf(1))
        input += pkt(apid, 1, false, byteArrayOf(1))
        input += unit(5, 2)
        input += tail()
        val out = run(f, input)
        assertEquals(2, out.count { TsContinuityDetector.pidOf(it, 0) == apid })
    }

    @Test fun startCodeStraddlingPacketsIsFound() {
        val f = TsIdrFilter()
        run(f, unit(5, 2)) // learn the video PID
        val a = unit(1, 3, skip = 1) // gap: recovering
        run(f, a)
        // IDR unit whose NAL start code is split across two TS packets
        val c1 = cc; cc = (cc + 1) and 0xf
        val c2 = cc; cc = (cc + 1) and 0xf
        val p1 = pkt(vpid, c1, true, ByteArray(180) { if (it >= 177) 0 else 0x55 }) // start code ends at the packet end
        val p2 = pkt(vpid, c2, false, byteArrayOf(1, 5, 1, 2))
        val out = run(f, listOf(p1, p2) + tail())
        assertEquals(2, out.size)
        assertFalse(f.recovering)
    }

    @Test fun resyncStartsRecoveryAfterVideoPidKnown() {
        var now = 0L
        val f = TsIdrFilter { now }
        assertFalse(f.resync()) // no video PID yet: nothing to discard
        assertFalse(f.recovering)
        run(f, unit(5, 2))
        now = 10_000
        assertTrue(f.resync())
        assertTrue(f.recovering)
        val out = run(f, unit(1, 2) + unit(5, 2) + tail())
        assertEquals(2, out.size) // only the IDR unit
    }
}

private class FakeLink(private val script: (Int) -> Any) : SrtLink {
    @Volatile var closed = false
    val calls = AtomicInteger(0)
    override fun recv(buf: ByteArray): Int {
        val n = calls.incrementAndGet()
        if (closed) throw IOException("closed")
        Thread.sleep(5)
        return when (val r = script(n)) {
            is SrtTimeout -> throw r
            is IOException -> throw r
            is ByteArray -> { System.arraycopy(r, 0, buf, 0, r.size); r.size }
            else -> throw IllegalStateException()
        }
    }
    override fun close() { closed = true }
}

class SrtReaderTest {
    @Test fun silentPeerKeepsLoopingAndCancelEndsIt() {
        val link = FakeLink { SrtTimeout() } // connected, sends nothing
        val reader = SrtReader(link)
        val done = CountDownLatch(1)
        var result = 0
        val t = Thread { result = reader.read(ByteArray(10), 0, 10); done.countDown() }
        t.start()
        Thread.sleep(100)
        assertTrue(link.calls.get() > 3) // looping on timeouts, not blocked forever
        assertEquals(1L, done.count)
        reader.cancel() // teardown on stop or a new LOAD
        assertTrue(done.await(2, TimeUnit.SECONDS))
        assertEquals(-1, result)
        assertTrue(link.closed)
    }

    @Test fun interruptionEndsTheRead() {
        val reader = SrtReader(FakeLink { SrtTimeout() })
        var thrown: Throwable? = null
        val t = Thread { try { reader.read(ByteArray(1), 0, 1) } catch (e: Throwable) { thrown = e } }
        t.start(); Thread.sleep(50); t.interrupt(); t.join(2000)
        assertNotNull(thrown)
    }

    @Test fun messagesAreServedInPiecesAndFilteredAwayMessagesAreSkipped() {
        val msgs = listOf(byteArrayOf(1, 2, 3, 4, 5), byteArrayOf(9, 9), byteArrayOf(6, 7))
        val link = FakeLink { n -> if (n <= 3) msgs[n - 1] else IOException("end") }
        val reader = SrtReader(link) { b, n -> if (b[0].toInt() == 9) ByteArray(0) else b.copyOf(n) }
        val out = ArrayList<Int>()
        val buf = ByteArray(2)
        try {
            while (true) { val n = reader.read(buf, 0, 2); if (n < 0) break; for (i in 0 until n) out.add(buf[i].toInt()) }
        } catch (e: IOException) { }
        assertEquals(listOf(1, 2, 3, 4, 5, 6, 7), out)
    }

    @Test fun brokenLinkThrows() {
        val reader = SrtReader(FakeLink { IOException("lost") })
        try { reader.read(ByteArray(1), 0, 1); throw AssertionError("no throw") } catch (e: IOException) { }
    }
}

internal class SlowHttp : Http {
    val started = AtomicInteger(0)
    val posts = java.util.concurrent.CopyOnWriteArrayList<String>()
    val gets = AtomicInteger(0)
    @Volatile var postDelayMs = 0L
    val release = CountDownLatch(1)
    override fun get(url: String, scope: CallScope): HttpResult { started.incrementAndGet(); gets.incrementAndGet(); return HttpResult(200, "", null) }
    override fun post(url: String, json: String, scope: CallScope): HttpResult {
        started.incrementAndGet()
        posts.add(json)
        if (postDelayMs > 0) scope.sleep(postDelayMs) else release.await(2, TimeUnit.SECONDS)
        return HttpResult(200, "", null)
    }
}

class NetWorkerTest {
    @Test fun staleGenerationIsDroppedBeforeTheRequestStarts() {
        val gate = GenerationGate()
        val http = SlowHttp()
        val w = NetWorker(gate, http)
        val blocker = CountDownLatch(1)
        val g1 = gate.next()
        w.run(g1) { blocker.await(2, TimeUnit.SECONDS) } // occupies the one thread
        Thread.sleep(30)
        w.run(g1) { http.get("u", it) }                  // queued behind it
        gate.next()                                      // a new LOAD replaces the generation
        blocker.countDown()
        Thread.sleep(150)
        assertEquals(0, http.started.get())
        w.dispose()
    }

    @Test fun queueIsBoundedUnderRepeatedLoads() {
        val gate = GenerationGate()
        val w = NetWorker(gate, SlowHttp(), queueCapacity = 4)
        val hold = CountDownLatch(1)
        val g = gate.next()
        var accepted = 0
        repeat(100) { if (w.run(g) { hold.await(2, TimeUnit.SECONDS) }) accepted += 1 }
        assertTrue("accepted $accepted", accepted <= 5) // 1 running + 4 queued
        assertTrue(w.rejected.get() >= 95)
        hold.countDown(); w.dispose()
    }

    @Test fun reportsCoalesceToTheNewest() {
        val gate = GenerationGate()
        val http = SlowHttp()
        val w = NetWorker(gate, http)
        val g = gate.next()
        w.report(g, "u", "r1") // goes in flight, blocks in post
        Thread.sleep(50)
        w.report(g, "u", "r2"); w.report(g, "u", "r3"); w.report(g, "u", "r4")
        http.release.countDown()
        Thread.sleep(300)
        assertEquals(listOf("r1", "r4"), http.posts.toList()) // one slot: the newest replaced the pending ones
        w.dispose()
    }

    @Test fun cancelAllCancelsInFlightCallsAndDropsPending() {
        val gate = GenerationGate()
        val w = NetWorker(gate, SlowHttp())
        val g = gate.next()
        val cancelled = CountDownLatch(1)
        w.run(g) { s -> s.register { cancelled.countDown() }; s.sleep(5000) }
        Thread.sleep(50)
        w.cancelAll()
        assertTrue(cancelled.await(1, TimeUnit.SECONDS))
        w.dispose()
    }

    @Test fun disposeShutsTheThreadDownAndRefusesWork() {
        val gate = GenerationGate()
        val w = NetWorker(gate, SlowHttp())
        assertTrue(w.run(gate.next()) { })
        w.dispose()
        assertTrue(w.awaitTermination(2000))
        assertTrue(w.isShutdown)
        assertFalse(w.run(gate.current) { })
    }

    @Test fun repeatedStartStopKeepsOneThread() {
        val before = Thread.getAllStackTraces().keys.count { it.name == "cast-net-test" }
        val gate = GenerationGate()
        val w = NetWorker(gate, SlowHttp(), threadName = "cast-net-test")
        repeat(50) { val g = gate.next(); w.run(g) { }; w.cancelAll(); gate.invalidate() }
        Thread.sleep(100)
        val during = Thread.getAllStackTraces().keys.count { it.name == "cast-net-test" }
        assertTrue(during - before <= 1)
        w.dispose(); assertTrue(w.awaitTermination(2000))
        assertEquals(before, Thread.getAllStackTraces().keys.count { it.name == "cast-net-test" })
    }

    @Test fun callScopeSleepWakesOnCancel() {
        val s = CallScope()
        Thread { Thread.sleep(50); s.cancel() }.start()
        val t0 = System.nanoTime()
        assertFalse(s.sleep(5000))
        assertTrue(System.nanoTime() - t0 < 2_000_000_000L)
    }

    // R2-A: a report queued behind a slow task and then made stale must not wedge reporting for later LOADs.
    @Test fun reportForTheNewGenerationPostsAfterAStaleQueuedReport() {
        val gate = GenerationGate()
        val http = SlowHttp()
        val w = NetWorker(gate, http)
        val g1 = gate.next()
        val gate1 = CountDownLatch(1)
        w.run(g1) { gate1.await(3, TimeUnit.SECONDS) } // a slow task occupies the worker
        Thread.sleep(30)
        w.report(g1, "u", "old-report")                // a report is queued
        gate.invalidate(); w.cancelAll()               // stop() / a new LOAD
        gate1.countDown()                              // release the worker
        Thread.sleep(100)
        val g2 = gate.next()
        http.release.countDown()
        w.report(g2, "u", "new-report")
        Thread.sleep(300)
        assertEquals(listOf("new-report"), http.posts.toList())
        w.dispose()
    }

    // R2-B: continuous slow reports must not starve control work.
    @Test fun controlWorkStartsWhileSlowReportsKeepComing() {
        val gate = GenerationGate()
        val http = SlowHttp().also { it.postDelayMs = 400 }
        val w = NetWorker(gate, http)
        val g = gate.next()
        val ran = CountDownLatch(1)
        val stop = java.util.concurrent.atomic.AtomicBoolean(false)
        val feeder = Thread { var i = 0; while (!stop.get()) { w.report(g, "u", "r${i++}"); Thread.sleep(20) } }
        feeder.start()
        Thread.sleep(100) // a report is in flight and another is pending
        val t0 = System.nanoTime()
        w.run(g) { ran.countDown() } // a token check or failure ladder call
        assertTrue(ran.await(2, TimeUnit.SECONDS))
        // At most the one report in flight (400 ms) is waited for, never the stream of reports.
        assertTrue((System.nanoTime() - t0) < 900_000_000L)
        stop.set(true); feeder.join(); w.dispose()
    }

    // R2-D: cancelAll removes obsolete tasks, so a burst of replacement LOADs always admits the newest negotiation.
    @Test fun burstOfLoadsAdmitsTheNewestNegotiation() {
        val gate = GenerationGate()
        val w = NetWorker(gate, SlowHttp(), queueCapacity = 2)
        val hold = CountDownLatch(1)
        val first = gate.next()
        w.run(first) { hold.await(3, TimeUnit.SECONDS) }
        Thread.sleep(30)
        var last = 0
        val ran = java.util.concurrent.CopyOnWriteArrayList<Int>()
        repeat(20) { // 20 LOADs in a burst: each start() is gate.next() + cancelAll() + run()
            gate.invalidate(); w.cancelAll()
            last = gate.next()
            val g = last
            assertTrue("negotiation of load $it admitted", w.run(g) { ran.add(g) })
        }
        hold.countDown()
        Thread.sleep(300)
        assertEquals(listOf(last), ran.toList()) // only the newest ran; the obsolete ones were removed
        w.dispose()
    }
}

class CallScopeHooksTest {
    @Test fun completedCallsLeaveNoHooks() {
        val s = CallScope()
        repeat(5000) { s.register { }.close() }
        assertEquals(0, s.hookCount)
    }

    @Test fun handleClosesExactlyOnceAndRaceSafely() {
        val s = CallScope()
        val h = s.register { }
        val ts = (1..8).map { Thread { h.close() } }
        ts.forEach { it.start() }; ts.forEach { it.join() }
        assertEquals(0, s.hookCount)
        h.close(); assertEquals(0, s.hookCount)
    }

    @Test fun cancelRunsHooksOffTheCallerThread() {
        val s = CallScope()
        val ran = CountDownLatch(1)
        var thread: String? = null
        s.register { thread = Thread.currentThread().name; ran.countDown() }
        s.cancel()
        assertTrue(ran.await(2, TimeUnit.SECONDS))
        assertEquals("cast-cancel", thread)
        assertEquals(0, s.hookCount)
    }

    @Test fun registerAfterCancelRunsTheHookAtOnce() {
        val s = CallScope()
        s.cancel()
        val ran = CountDownLatch(1)
        s.register { ran.countDown() }
        assertTrue(ran.await(2, TimeUnit.SECONDS))
        assertEquals(0, s.hookCount)
    }

    @Test fun closedHandleIsNotRunOnCancel() {
        val s = CallScope()
        val ran = AtomicInteger(0)
        s.register { ran.incrementAndGet() }.close()
        s.cancel()
        Thread.sleep(100)
        assertEquals(0, ran.get())
    }
}

class TokenCheckTest {
    @Test fun staleFinishNeverClearsTheNewGenerationsState() {
        val c = TokenCheck()
        assertTrue(c.begin(1))      // check for LOAD 1 in flight
        c.reset()                   // stop(): per-generation state is reset
        assertTrue(c.begin(2))      // revoked-token HLS on the replacement LOAD
        c.finish(1)                 // the old check's posted reset arrives late
        assertFalse(c.begin(2))     // LOAD 2's check is still the one in flight
        c.finish(2)
        assertTrue(c.begin(2))
    }

    @Test fun secondBeginForTheSameGenerationIsRefused() {
        val c = TokenCheck()
        assertTrue(c.begin(7)); assertFalse(c.begin(7))
    }
}

class LadderStrictTest {
    private fun srt(extra: String) =
        """{"v":1,"attach":{"rung":"srt","host":"h.example","port":9010,"sid":"$SID"$extra}}"""

    private fun unusable(body: String) = assertEquals(LadderAnswer.Unusable, LadderParser.parse(200, body))

    @Test fun portOverflowAndWrongTypes() {
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":4294976306,"sid":"$SID"}}""")
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":"9010","sid":"$SID"}}""")
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":9010.5,"sid":"$SID"}}""")
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":true,"sid":"$SID"}}""")
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":65536,"sid":"$SID"}}""")
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":-1,"sid":"$SID"}}""")
    }

    @Test fun vMustBeTheInteger1() {
        unusable("""{"v":"1","attach":{"rung":"hls"}}""")
        unusable("""{"v":1.5,"attach":{"rung":"hls"}}""")
        unusable("""{"v":true,"attach":{"rung":"hls"}}""")
    }

    @Test fun latencyAndExpiryBounds() {
        unusable(srt(""","latency_ms":19"""))
        unusable(srt(""","latency_ms":8001"""))
        unusable(srt(""","latency_ms":"250""""))
        unusable(srt(""","latency_ms":99999999999"""))
        unusable(srt(""","expires_in_ms":0"""))
        unusable(srt(""","expires_in_ms":99999999999"""))
        unusable(srt(""","expires_in_ms":"30000""""))
        val ok = LadderParser.parse(200, srt(""","latency_ms":20,"expires_in_ms":3600000""")) as LadderAnswer.Srt
        assertEquals(20, ok.latencyMs)
    }

    @Test fun hostValidation() {
        for (h in listOf("", " ", "a b", "h\\u0000", "h\\n", "-a.example", "a..b", "a".repeat(64) + ".example",
            "x.".repeat(130) + "x", "h/ost", "h:ost", "h@ost", "[::1]", "h\\u00e9.example")) {
            unusable("""{"v":1,"attach":{"rung":"srt","host":"$h","port":9010,"sid":"$SID"}}""")
        }
        for (h in listOf("h.example", "a-b.example.com", "10.1.2.3", "2001:db8::1", "::1")) {
            assertTrue(h, LadderParser.parse(200, """{"v":1,"attach":{"rung":"srt","host":"$h","port":9010,"sid":"$SID"}}""") is LadderAnswer.Srt)
        }
    }

    @Test fun sidMustBeExactly22Base64urlChars() {
        for (s in listOf("", "a".repeat(21), "a".repeat(23), "a".repeat(21) + "+", "a".repeat(21) + "=", "a".repeat(21) + " ")) {
            unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":9010,"sid":"$s"}}""")
        }
        unusable("""{"v":1,"attach":{"rung":"srt","host":"h","port":9010,"sid":12345}}""")
    }

    @Test fun malformedAttachPlaysHls() {
        unusable("""{"v":1,"attach":"srt"}""")
        assertEquals(LadderAnswer.Hls, LadderParser.parse(200, """{"v":1,"attach":{"rung":5}}"""))
    }
}

class LossCountingTest {
    private val vpid = 0x100
    private var cc = 0
    private fun pkt(counter: Int, start: Boolean, type: Int = 1): ByteArray {
        val p = ByteArray(188) { 0xff.toByte() }
        p[0] = 0x47; p[1] = ((if (start) 0x40 else 0) or (vpid shr 8)).toByte(); p[2] = (vpid and 0xff).toByte()
        p[3] = (0x10 or (counter and 0xf)).toByte()
        if (start) { p[4] = 0; p[5] = 0; p[6] = 1; p[7] = 0xe0.toByte(); p[8] = 0; p[9] = 0; p[10] = 0; p[11] = 1; p[12] = type.toByte() }
        return p
    }
    private fun unit(type: Int, lose: Boolean): List<ByteArray> {
        val a = cc; cc = (cc + 1) and 15
        val b = cc; cc = (cc + 1) and 15
        return if (lose) listOf(pkt(a, true, type)) else listOf(pkt(a, true, type), pkt(b, false))
    }
    private fun feed(f: TsIdrFilter, l: List<ByteArray>) = l.forEach { f.process(it, 0, 188) }

    @Test fun oneLossSeenByBothMechanismsCountsOnce() {
        var now = 0L
        val f = TsIdrFilter { now }
        feed(f, unit(5, false) + unit(1, false) + unit(1, true) + unit(1, false)) // a gap: recovery 1
        assertEquals(1, f.recoveries)
        now += 200
        assertFalse(f.resync()) // the drop counter rises for the same loss: merged, not counted
        feed(f, unit(5, false) + unit(1, false) + unit(1, false)) // the IDR ends the recovery
        now += 300
        assertFalse(f.resync()) // evidence arriving on a later receive, shortly after the recovery: still merged
        assertEquals(1, f.recoveries)
    }

    @Test fun threeRecoveriesDoNotStepDownTheFourthDoes() {
        var now = 0L
        val f = TsIdrFilter { now }
        val policy = LossPolicy()
        val actions = ArrayList<LossAction>()
        feed(f, unit(5, false) + unit(1, false))
        repeat(4) {
            val before = f.recoveries
            feed(f, unit(1, true) + unit(1, false) + unit(5, false) + unit(1, false) + unit(1, false))
            if (f.recoveries != before) actions.add(policy.onLoss(now))
            now += 5_000 // real, separate losses
        }
        assertEquals(
            listOf(LossAction.CONTINUE_IN_CONNECTION, LossAction.CONTINUE_IN_CONNECTION, LossAction.CONTINUE_IN_CONNECTION, LossAction.STEP_DOWN),
            actions,
        )
    }
}

class Ipv6LiteralTest {
    @Test fun strictShape() {
        for (h in listOf("::1", "2001:db8::1", "fe80::1", "::ffff:10.1.2.3", "1:2:3:4:5:6:7:8", "2001:db8:0:0:0:0:0:1")) {
            assertTrue(h, LadderParser.validIpv6Literal(h))
        }
        for (h in listOf(":::", "1::2::3", "1:2:3:4:5:6:7:8:9", "12345::1", "g::1", "::1%eth0", "[::1]", "1.2.3.4",
            "::256.1.1.1", "::1.2.3", ":", "::1::", "1:2:3:4:5:6:7::8:9")) {
            assertFalse(h, LadderParser.validIpv6Literal(h))
        }
    }
}
