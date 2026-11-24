package studio.conspect.cast.tv

/**
 * Detects a gap in the MPEG-TS continuity counter of the video PID (ADR-0114 "Receiver buffers", recovery).
 * Feed whole 188-byte packets in order. A gap means loss.
 */
class TsContinuityDetector {
    var videoPid = -1
        private set
    private var last = -1

    /** Number of gaps seen. */
    var gaps = 0
        private set

    /** Feeds a buffer of whole TS packets; returns true if a gap was found in this buffer. */
    fun feed(buf: ByteArray, offset: Int, length: Int): Boolean {
        var gap = false
        var i = offset
        val end = offset + length
        while (i + PACKET <= end) {
            if (buf[i] != SYNC) {
                i += 1 // resync byte by byte
                continue
            }
            if (packet(buf, i)) gap = true
            i += PACKET
        }
        return gap
    }

    /** One packet at [i]. True when it is a video packet that follows a gap. */
    fun packet(buf: ByteArray, i: Int): Boolean {
        val pid = pidOf(buf, i)
        val afc = (buf[i + 3].toInt() shr 4) and 3
        val cc = buf[i + 3].toInt() and 0x0f
        val payloadStart = (buf[i + 1].toInt() and 0x40) != 0
        if (videoPid < 0 && payloadStart && pid >= 0x20 && afc and 1 != 0 && isVideoPesStart(buf, i, afc)) {
            videoPid = pid
        }
        if (pid != videoPid || afc and 1 == 0) return false // CC increments only on packets with payload
        val gap = last >= 0 && cc != (last + 1) and 0x0f && cc != last
        if (gap) gaps += 1
        last = cc
        return gap
    }

    private fun isVideoPesStart(buf: ByteArray, i: Int, afc: Int): Boolean {
        val p = payloadOffset(buf, i, afc)
        if (p + 4 > i + PACKET) return false
        if (buf[p].toInt() != 0 || buf[p + 1].toInt() != 0 || buf[p + 2].toInt() != 1) return false
        return (buf[p + 3].toInt() and 0xf0) == 0xe0 // video stream ids 0xE0..0xEF
    }

    fun reset() {
        videoPid = -1
        last = -1
    }

    companion object {
        const val PACKET = 188
        const val SYNC = 0x47.toByte()

        fun pidOf(buf: ByteArray, i: Int): Int = ((buf[i + 1].toInt() and 0x1f) shl 8) or (buf[i + 2].toInt() and 0xff)

        fun payloadOffset(buf: ByteArray, i: Int, afc: Int): Int {
            var p = i + 4
            if (afc and 2 != 0) p += 1 + (buf[p].toInt() and 0xff)
            return p
        }
    }
}

/**
 * In-connection loss recovery with discard-to-IDR (ADR-0114 "Receiver buffers", recovery; the socket and its
 * one-time sid are kept). Sits between the SRT socket and the TS extractor, on whole packets.
 *
 * Video packets are held per access unit (PES): the unit is released when the next unit starts. That costs no
 * latency, because TsExtractor and H264Reader emit a sample only when the next unit starts too. A unit that had a
 * continuity gap inside it is never released. After a gap, every video unit is dropped until one contains an IDR
 * slice (NAL type 5); that unit and everything after it flow again. Audio and other PIDs always pass: every
 * audio frame is independently decodable. The extractor therefore never sees a frame that depends on lost data.
 * The held unit is bounded by [MAX_UNIT_BYTES]; an oversized unit is discarded through the next PES start so later
 * continuity loss cannot retract data that has already reached the extractor.
 */
class TsIdrFilter(private val nowMs: () -> Long = { System.nanoTime() / 1_000_000L }) {
    private var lastRecoveryMs = Long.MIN_VALUE / 2
    private val detector = TsContinuityDetector()
    private var dropping = false
    private val pending = java.io.ByteArrayOutputStream()
    private val payload = java.io.ByteArrayOutputStream() // payload bytes only: a start code can straddle TS headers
    private var scanFrom = 0
    private var idrSeen = false
    private var discardUntilStart = false
    // SRT message boundaries are independent of 188-byte MPEG-TS packet boundaries.
    // Retain at most one incomplete packet; never bypass continuity on a short read.
    private val partialPacket = ByteArray(TsContinuityDetector.PACKET)
    private var partialLength = 0

    /** Gaps that started a recovery. */
    var recoveries = 0
        private set

    val recovering: Boolean get() = dropping

    /**
     * libsrt's receive-drop counter rose. The same loss is usually also a continuity gap, so this is merged into a
     * recovery that is going on, or that began or ended less than [MERGE_MS] ago. Returns true when it started a
     * recovery of its own.
     */
    fun resync(): Boolean {
        if (detector.videoPid < 0 || dropping || nowMs() - lastRecoveryMs < MERGE_MS) return false
        beginDropping()
        return true
    }

    private fun beginDropping() {
        dropping = true
        lastRecoveryMs = nowMs()
        recoveries += 1
        clearPending()
    }

    private fun clearPending() {
        pending.reset()
        payload.reset()
        scanFrom = 0
        idrSeen = false
    }

    /**
     * Reassembles TS packets across arbitrary SRT message boundaries. Only complete, sync-aligned packets
     * reach continuity and IDR filtering. At most 187 trailing bytes are retained between calls; invalid
     * sync alignment fails closed into recovery instead of passing unchecked video to the extractor.
     */
    fun process(buf: ByteArray, off: Int, len: Int): ByteArray {
        require(off >= 0 && len >= 0 && off <= buf.size - len)
        val out = java.io.ByteArrayOutputStream(len + TsContinuityDetector.PACKET)
        var i = off
        val end = off + len
        while (i < end) {
            if (partialLength == 0 && buf[i] != TsContinuityDetector.SYNC) {
                // No reliable packet boundary: discard until the next TS sync.
                // A damaged video packet cannot be emitted before an intact IDR.
                if (!dropping) beginDropping()
                i += 1
                continue
            }
            val count = minOf(TsContinuityDetector.PACKET - partialLength, end - i)
            System.arraycopy(buf, i, partialPacket, partialLength, count)
            partialLength += count
            i += count
            if (partialLength == TsContinuityDetector.PACKET) {
                if (partialPacket[0] == TsContinuityDetector.SYNC) {
                    packet(partialPacket, 0, out)
                } else {
                    beginDropping()
                }
                partialLength = 0
            }
        }
        return out.toByteArray()
    }

    private fun packet(buf: ByteArray, i: Int, out: java.io.ByteArrayOutputStream) {
        val gap = detector.packet(buf, i)
        val pid = TsContinuityDetector.pidOf(buf, i)
        if (pid != detector.videoPid) {
            out.write(buf, i, TsContinuityDetector.PACKET)
            return
        }
        val pusi = (buf[i + 1].toInt() and 0x40) != 0
        if (gap) {
            // The unit in progress lost data: it is never released. Recovery starts (or restarts inside a candidate).
            if (!dropping) {
                beginDropping()
            } else {
                clearPending()
            }
        }
        if (pusi) {
            if (!dropping && !discardUntilStart) pending.writeTo(out) // the previous unit is complete and intact
            pending.reset()
            payload.reset()
            scanFrom = 0
            idrSeen = false
            discardUntilStart = false
        }
        if (discardUntilStart) return // discard the rest of the oversized unit, including any later-lost packets
        if (!pusi && pending.size() == 0) {
            // the tail of a unit that was never released (a gap, or a unit dropped in recovery)
            if (!dropping) out.write(buf, i, TsContinuityDetector.PACKET) // before the first unit: nothing to hold
            return
        }
        pending.write(buf, i, TsContinuityDetector.PACKET)
        if (dropping) {
            val afc = (buf[i + 3].toInt() shr 4) and 3
            if (afc and 1 != 0) {
                val q = TsContinuityDetector.payloadOffset(buf, i, afc)
                if (q < i + TsContinuityDetector.PACKET) payload.write(buf, q, i + TsContinuityDetector.PACKET - q)
            }
            scanForIdr()
            if (idrSeen) { // the candidate unit becomes the held unit of normal flow
                dropping = false
                lastRecoveryMs = nowMs()
                payload.reset()
                scanFrom = 0
                idrSeen = false
            }
        }
        if (pending.size() > MAX_UNIT_BYTES) {
            // Never emit an access unit whose completeness cannot be established after loss.
            // Bound memory by discarding the unit until the next PES start, then resume buffering.
            clearPending()
            discardUntilStart = true
        }
    }

    /** IDR slice: start code 00 00 01 followed by a NAL header with type 5. */
    private fun scanForIdr() {
        val b = payload.toByteArray()
        var k = maxOf(scanFrom - 3, 0)
        while (k + 3 < b.size) {
            if (b[k].toInt() == 0 && b[k + 1].toInt() == 0 && b[k + 2].toInt() == 1 && (b[k + 3].toInt() and 0x1f) == 5) {
                idrSeen = true
                return
            }
            k += 1
        }
        scanFrom = b.size
    }

    fun reset() {
        detector.reset()
        dropping = false
        lastRecoveryMs = Long.MIN_VALUE / 2
        clearPending()
        discardUntilStart = false
        partialLength = 0
        recoveries = 0
    }

    companion object {
        const val MAX_UNIT_BYTES = 1024 * 1024
        const val MERGE_MS = 1000L
    }
}
