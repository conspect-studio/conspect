package studio.conspect.cast.tv

import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

class HttpResult(val status: Int, val body: String?, val retryAfter: String?)

/** Disconnects run here, never on the main thread. */
internal object CancelExecutor {
    val exec: java.util.concurrent.ExecutorService = Executors.newSingleThreadExecutor { r ->
        Thread(r, "cast-cancel").apply { isDaemon = true }
    }
}

/**
 * Work of one cast generation: cancelling it disconnects every call registered on it (on a worker thread, not the
 * caller's) and wakes its sleeps. A registration is a [Handle]: closing it removes the hook exactly once.
 */
class CallScope {
    @Volatile
    var cancelled = false
        private set
    private val latch = CountDownLatch(1)
    private val lock = Any()
    private val hooks = LinkedHashMap<Handle, () -> Unit>()

    inner class Handle internal constructor() : AutoCloseable {
        private val done = java.util.concurrent.atomic.AtomicBoolean(false)
        override fun close() {
            if (done.compareAndSet(false, true)) synchronized(lock) { hooks.remove(this) }
        }
        internal fun markDone() = done.set(true)
    }

    /** Registers [hook]; runs it (off-thread) at once when the scope is already cancelled. Close the handle when done. */
    fun register(hook: () -> Unit): Handle {
        val h = Handle()
        val runNow: Boolean
        synchronized(lock) {
            runNow = cancelled
            if (!runNow) hooks[h] = hook
        }
        if (runNow) {
            h.markDone()
            CancelExecutor.exec.execute { runCatching(hook) }
        }
        return h
    }

    val hookCount: Int get() = synchronized(lock) { hooks.size }

    fun cancel() {
        val toRun: List<() -> Unit>
        synchronized(lock) {
            if (cancelled) return
            cancelled = true
            toRun = hooks.values.toList()
            hooks.keys.forEach { it.markDone() }
            hooks.clear()
        }
        latch.countDown()
        toRun.forEach { h -> CancelExecutor.exec.execute { runCatching(h) } }
    }

    /** Sleeps up to [ms]; returns false when the scope was cancelled meanwhile. */
    fun sleep(ms: Long): Boolean = !latch.await(ms, TimeUnit.MILLISECONDS)
}

interface Http {
    fun get(url: String, scope: CallScope): HttpResult
    fun post(url: String, json: String, scope: CallScope): HttpResult
}

/**
 * HttpURLConnection client. Errors give status 0 and never carry the URL (it holds the token). Each call has a
 * total deadline (connect, send and read together), not only per-read timeouts, and is disconnected on cancel.
 */
object UrlHttp : Http {
    private const val TIMEOUT_MS = 5000
    const val TOTAL_DEADLINE_MS = 8000L
    private const val MAX_BODY = 256 * 1024

    private val deadlines: ScheduledExecutorService = Executors.newScheduledThreadPool(1) { r ->
        Thread(r, "cast-http-deadline").apply { isDaemon = true }
    }

    override fun get(url: String, scope: CallScope): HttpResult = call(url, null, scope)
    override fun post(url: String, json: String, scope: CallScope): HttpResult = call(url, json, scope)

    private fun call(url: String, json: String?, scope: CallScope): HttpResult {
        if (scope.cancelled) return HttpResult(0, null, null)
        val conn: HttpURLConnection
        val handle: CallScope.Handle
        val deadline: java.util.concurrent.ScheduledFuture<*>
        try {
            conn = URL(url).openConnection() as HttpURLConnection
            handle = scope.register { conn.disconnect() }
            deadline = deadlines.schedule({ conn.disconnect() }, TOTAL_DEADLINE_MS, TimeUnit.MILLISECONDS)
        } catch (e: Exception) {
            return HttpResult(0, null, null)
        }
        return try {
            conn.connectTimeout = TIMEOUT_MS
            conn.readTimeout = TIMEOUT_MS
            conn.instanceFollowRedirects = false
            conn.setRequestProperty("User-Agent", "Conspect-AndroidTV")
            if (json != null) {
                conn.requestMethod = "POST"
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json")
                conn.outputStream.use { it.write(json.toByteArray(Charsets.UTF_8)) }
            }
            val status = conn.responseCode
            val stream = if (status >= 400) conn.errorStream else conn.inputStream
            val body = stream?.use { readBounded(it, MAX_BODY) }
            HttpResult(status, body, conn.getHeaderField("Retry-After"))
        } catch (e: Exception) {
            HttpResult(0, null, null)
        } finally {
            deadline.cancel(false)
            handle.close() // the hook list never grows past the calls in flight
            conn.disconnect()
        }
    }

    private fun readBounded(s: java.io.InputStream, max: Int): String {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(8192)
        while (out.size() < max) {
            val n = s.read(buf)
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toString("UTF-8")
    }
}
