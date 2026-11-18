package studio.conspect.cast.tv

import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

class HttpResult(val status: Int, val body: String?, val retryAfter: String?)

/** Work of one cast generation: cancelling it disconnects every call registered on it and wakes its sleeps. */
class CallScope {
    @Volatile
    var cancelled = false
        private set
    private val latch = CountDownLatch(1)
    private val hooks = CopyOnWriteArrayList<() -> Unit>()

    fun onCancel(hook: () -> Unit) {
        hooks.add(hook)
        if (cancelled) hook()
    }

    fun cancel() {
        if (cancelled) return
        cancelled = true
        latch.countDown()
        hooks.forEach { runCatching(it) }
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
        var c: HttpURLConnection? = null
        val deadline = try {
            c = URL(url).openConnection() as HttpURLConnection
            val conn = c
            scope.onCancel { conn.disconnect() }
            deadlines.schedule({ conn.disconnect() }, TOTAL_DEADLINE_MS, TimeUnit.MILLISECONDS)
        } catch (e: Exception) {
            return HttpResult(0, null, null)
        }
        return try {
            val conn = c!!
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
            c?.disconnect()
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
