package studio.conspect.cast.tv

import org.json.JSONObject

/** The receiver identity sent in ladder calls and reports. */
data class ReceiverInfo(val build: String, val sdkInt: Int) {
    val kind: String get() = "android"
    val ua: String get() = "Conspect-AndroidTV/$build Android/$sdkInt"
}

data class LadderFailure(val stage: String, val ms: Long) {
    init {
        require(stage == "connect" || stage == "first-frame" || stage == "stall")
    }
}

object LadderRequest {
    /**
     * The `POST /cast/<token>/ladder` body (decision 3). [libsrt] is the loaded libsrt version string, or null
     * when it could not be loaded; [skipSrtSeconds] is the seconds left in the failure memory (0: not skipped).
     */
    fun body(
        receiver: ReceiverInfo,
        libsrt: String?,
        skipSrtSeconds: Int,
        failed: LadderFailure?,
        clockT0Us: Long? = null,
    ): String {
        val o = JSONObject()
        o.put("v", 1)
        o.put(
            "receiver",
            JSONObject().put("kind", receiver.kind).put("build", receiver.build).put("ua", receiver.ua.take(200)),
        )
        o.put(
            "caps",
            JSONObject()
                .put("srt", libsrt ?: JSONObject.NULL)
                .put("frames", JSONObject.NULL)
                .put("wt", false).put("mse", false).put("rtc", false).put("rvfc", false),
        )
        o.put("need", JSONObject().put("subtitles", false).put("audio", JSONObject.NULL))
        o.put("skip", if (skipSrtSeconds > 0) JSONObject().put("srt", skipSrtSeconds) else JSONObject())
        o.put(
            "failed",
            if (failed == null) JSONObject.NULL
            else JSONObject().put("rung", "srt").put("stage", failed.stage).put("ms", failed.ms),
        )
        if (clockT0Us != null) o.put("clock", JSONObject().put("t0", clockT0Us))
        return o.toString()
    }
}

/** What the engine said to a ladder call. */
sealed class LadderAnswer {
    data class Srt(
        val host: String,
        val port: Int,
        val latencyMs: Int,
        val sid: String,
        val expiresInMs: Long,
    ) : LadderAnswer()

    /** Play HLS: an `hls` attach, an unknown rung (moq, webrtc: two rungs only here), or an old engine. */
    data object Hls : LadderAnswer()

    /** 404: an engine without the ladder, or a dead token. The caller plays HLS; master.m3u8 404 ends the cast. */
    data object NotFound : LadderAnswer()

    /** 429: retry once after [retryAfterSeconds], else step down to HLS. */
    data class RateLimited(val retryAfterSeconds: Int) : LadderAnswer()

    /** Anything else (5xx, malformed body, v mismatch): HLS. */
    data object Unusable : LadderAnswer()
}

object LadderParser {
    const val DEFAULT_LATENCY_MS = 250
    private const val DEFAULT_RETRY_AFTER_S = 1
    private const val MAX_RETRY_AFTER_S = 5

    fun parse(status: Int, body: String?, retryAfterHeader: String? = null): LadderAnswer {
        if (status == 404) return LadderAnswer.NotFound
        if (status == 429) {
            val s = retryAfterHeader?.trim()?.toIntOrNull() ?: DEFAULT_RETRY_AFTER_S
            return LadderAnswer.RateLimited(s.coerceIn(0, MAX_RETRY_AFTER_S))
        }
        if (status != 200 || body == null) return LadderAnswer.Unusable
        return try {
            val o = JSONObject(body)
            if (intOf(o.opt("v")) != 1) return LadderAnswer.Unusable
            val attach = o.opt("attach") as? JSONObject ?: return LadderAnswer.Unusable
            when (attach.opt("rung") as? String) {
                "srt" -> srt(attach)
                else -> LadderAnswer.Hls
            }
        } catch (e: Exception) {
            LadderAnswer.Unusable
        }
    }

    /** An integral JSON number as Long; null for strings, booleans, fractions, null and out-of-range values. */
    internal fun longOf(v: Any?): Long? = when (v) {
        is Int -> v.toLong()
        is Long -> v
        is Double -> if (v == Math.rint(v) && Math.abs(v) < 9.0e15) v.toLong() else null
        is java.math.BigInteger -> if (v.bitLength() < 63) v.toLong() else null
        else -> null
    }

    internal fun intOf(v: Any?): Int? = longOf(v)?.takeIf { it in Int.MIN_VALUE..Int.MAX_VALUE }?.toInt()

    private val SID = Regex("^[A-Za-z0-9_-]{22}$")
    private val DNS_LABEL = Regex("^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$")

    /** A DNS name (at most 253 chars, labels of 1..63) or an IP literal; no whitespace or control characters. */
    internal fun validHost(h: String): Boolean {
        if (h.isEmpty() || h.length > 253) return false
        if (h.any { it.code <= 0x20 || it.code == 0x7f || it.code > 0x7e }) return false
        if (h.contains(':')) return Regex("^[0-9A-Fa-f:.]{2,45}$").matches(h) // IPv6 literal, no brackets
        return h.split('.').all { DNS_LABEL.matches(it) }
    }

    private fun srt(a: JSONObject): LadderAnswer {
        val host = a.opt("host") as? String ?: return LadderAnswer.Unusable
        val port = longOf(a.opt("port")) ?: return LadderAnswer.Unusable
        val sid = a.opt("sid") as? String ?: return LadderAnswer.Unusable
        if (!validHost(host) || port !in 1L..65535L || !SID.matches(sid)) return LadderAnswer.Unusable
        // Optional numbers: absent means the default; present but wrong type or range means a malformed answer.
        val latency = if (a.has("latency_ms")) {
            intOf(a.opt("latency_ms"))?.takeIf { it in 20..8000 } ?: return LadderAnswer.Unusable
        } else DEFAULT_LATENCY_MS
        val expires = if (a.has("expires_in_ms")) {
            longOf(a.opt("expires_in_ms"))?.takeIf { it in 1L..3_600_000L } ?: return LadderAnswer.Unusable
        } else 30_000L
        return LadderAnswer.Srt(host, port.toInt(), latency, sid, expires)
    }
}
