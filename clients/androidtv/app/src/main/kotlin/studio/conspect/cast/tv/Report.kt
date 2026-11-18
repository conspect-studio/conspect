package studio.conspect.cast.tv

import org.json.JSONObject

/**
 * The report of `receiver.js` plus `receiver.kind = "android-tv"`. The latency method for SRT is `unavailable`
 * in this build (ADR-0114 decision 2) until the instrumented test of slice 6 passes on a device; only
 * `first_frame_ms` is reported.
 */
object Report {
    const val SRT_LATENCY_METHOD = "unavailable"

    fun latencyMethod(rung: String): String = if (rung == "srt") SRT_LATENCY_METHOD else "unavailable"

    fun build(
        receiver: ReceiverInfo,
        state: String,
        rung: String?,
        firstFrameMs: Long?,
        decoded: Long?,
        dropped: Long?,
        stalls: Int,
        softResets: Int,
        errorMessage: String?,
        atMs: Long,
    ): String {
        val o = JSONObject()
        o.put("v", 1)
        o.put("state", state)
        o.put("error", if (errorMessage == null) JSONObject.NULL else JSONObject().put("message", errorMessage.take(200)))
        o.put(
            "stats",
            JSONObject().put("decoded", decoded ?: JSONObject.NULL).put("dropped", dropped ?: JSONObject.NULL)
                .put("stalls", stalls),
        )
        o.put("device", receiver.ua.take(200))
        o.put("receiver", JSONObject().put("kind", "android-tv").put("build", receiver.build))
        o.put(
            "transport",
            JSONObject()
                .put("rung", rung ?: JSONObject.NULL)
                .put("first_frame_ms", firstFrameMs ?: JSONObject.NULL)
                .put("latency_method", latencyMethod(rung ?: ""))
                .put("latency_ms", JSONObject.NULL)
                .put("clock_error_ms", JSONObject.NULL)
                .put("resets", JSONObject().put("soft", softResets).put("hard", 0).put("last", JSONObject.NULL))
                .put("stalled", state == "buffering"),
        )
        o.put("at_ms", atMs)
        return o.toString()
    }
}

/** Counts flushes in a sliding 60 s window; more than 3 steps down to HLS. */
class FlushBudget(private val limit: Int = 3, private val windowMs: Long = 60_000L) {
    private val times = ArrayDeque<Long>()

    /** Records a flush at [nowMs]; returns true when the budget is exceeded. */
    fun record(nowMs: Long): Boolean {
        times.addLast(nowMs)
        while (times.isNotEmpty() && nowMs - times.first() > windowMs) times.removeFirst()
        return times.size > limit
    }
}
