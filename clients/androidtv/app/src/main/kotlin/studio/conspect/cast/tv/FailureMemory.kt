package studio.conspect.cast.tv

import android.content.Context

fun interface Clock {
    fun nowMs(): Long
}

/** Persistence of `origin -> expiry`. Origin and expiry only: never a token. */
interface FailureStore {
    fun all(): Map<String, Long>
    fun put(origin: String, expiryMs: Long)
    fun remove(origin: String)
}

class PrefsFailureStore(context: Context) : FailureStore {
    private val prefs = context.getSharedPreferences("srt_failures", Context.MODE_PRIVATE)

    override fun all(): Map<String, Long> =
        prefs.all.mapNotNull { (k, v) -> (v as? Long)?.let { k to it } }.toMap()

    override fun put(origin: String, expiryMs: Long) {
        prefs.edit().putLong(origin, expiryMs).apply()
    }

    override fun remove(origin: String) {
        prefs.edit().remove(origin).apply()
    }
}

/** The SRT failure memory of ADR-0114 decision 2 (Selection, 5): 15 minutes per engine origin. */
class FailureMemory(private val store: FailureStore, private val clock: Clock) {
    fun remember(origin: String) {
        store.put(origin, clock.nowMs() + TTL_MS)
    }

    /** Seconds left for [origin], or 0 when there is no active entry. Expired entries are removed. */
    fun secondsLeft(origin: String): Int {
        val expiry = store.all()[origin] ?: return 0
        val left = expiry - clock.nowMs()
        if (left <= 0) {
            store.remove(origin)
            return 0
        }
        return ((left + 999) / 1000).toInt()
    }

    fun isActive(origin: String): Boolean = secondsLeft(origin) > 0

    /** True when any origin is in memory: the launch checker cannot see the origin of a LAUNCH. */
    fun anyActive(): Boolean = store.all().keys.toList().any { isActive(it) }

    companion object {
        const val TTL_MS: Long = 15L * 60L * 1000L
    }
}
