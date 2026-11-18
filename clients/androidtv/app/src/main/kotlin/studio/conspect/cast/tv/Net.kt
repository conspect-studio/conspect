package studio.conspect.cast.tv

import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

/**
 * The session's network work: one thread, a bounded queue, work tagged with a generation and dropped before it
 * starts when the generation is stale, cancellable through a [CallScope], and shut down by [dispose].
 * Reports are coalesced: at most one in flight, and the newest replaces a pending one.
 */
class NetWorker(
    private val gate: GenerationGate,
    private val http: Http,
    queueCapacity: Int = 8,
    threadName: String = "cast-net",
) {
    private val exec = ThreadPoolExecutor(
        1, 1, 0L, TimeUnit.MILLISECONDS, ArrayBlockingQueue(queueCapacity),
        { r -> Thread(r, threadName).apply { isDaemon = true } },
        ThreadPoolExecutor.AbortPolicy(),
    )

    @Volatile
    private var scope = CallScope()
    private val disposed = AtomicBoolean(false)
    val rejected = AtomicInteger(0)

    private class Pending(val gen: Int, val url: String, val json: String)

    private val pending = AtomicReference<Pending?>(null)
    private val inFlight = AtomicBoolean(false)

    /** Cancels everything in flight and queued (the old scope), and starts a fresh one. */
    fun cancelAll() {
        val old = scope
        scope = CallScope()
        old.cancel()
        pending.set(null)
    }

    /** Queues [task] for generation [gen]; false when the queue is full or the worker is disposed. */
    fun run(gen: Int, task: (CallScope) -> Unit): Boolean {
        if (disposed.get()) return false
        val s = scope
        return try {
            exec.execute { if (gate.isCurrent(gen) && !s.cancelled) task(s) }
            true
        } catch (e: RejectedExecutionException) {
            rejected.incrementAndGet()
            false
        }
    }

    /** POSTs [json] to [url]; a pending report is replaced by this newer one. */
    fun report(gen: Int, url: String, json: String) {
        pending.set(Pending(gen, url, json))
        if (inFlight.compareAndSet(false, true)) {
            if (!run(gen) { drain(it) }) inFlight.set(false)
        }
    }

    private fun drain(s: CallScope) {
        do {
            while (true) {
                val p = pending.getAndSet(null) ?: break
                if (gate.isCurrent(p.gen) && !s.cancelled) http.post(p.url, p.json, s)
            }
            inFlight.set(false)
        } while (pending.get() != null && inFlight.compareAndSet(false, true))
    }

    fun dispose() {
        if (!disposed.compareAndSet(false, true)) return
        scope.cancel()
        pending.set(null)
        exec.shutdownNow()
    }

    val isShutdown: Boolean get() = exec.isShutdown
}

/**
 * One token check at a time per generation. Touched from the main thread only. A check that finishes after its
 * generation was replaced never clears the new generation's state.
 */
class TokenCheck {
    private var gen = -1

    /** True when a check for [generation] may start. */
    fun begin(generation: Int): Boolean {
        if (gen == generation) return false
        gen = generation
        return true
    }

    fun finish(generation: Int) {
        if (gen == generation) gen = -1
    }

    fun reset() {
        gen = -1
    }
}
