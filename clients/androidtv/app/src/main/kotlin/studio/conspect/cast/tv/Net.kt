package studio.conspect.cast.tv

import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * The session's network work: one daemon thread, shut down by [dispose].
 *
 * - Control work (negotiation, token checks, ladder failure calls) has priority over reports and is bounded.
 * - There is one report slot: the newest report replaces a pending one, so reports never queue up and need no
 *   in-flight flag. The worker takes at most one report per turn and looks at control work again before the next.
 * - Every task carries its generation. A task whose generation is stale is dropped before it starts; [cancelAll]
 *   removes every queued task, empties the report slot, and cancels the calls in flight through their [CallScope].
 */
class NetWorker(
    private val gate: GenerationGate,
    private val http: Http,
    private val queueCapacity: Int = 8,
    threadName: String = "cast-net",
) {
    private class Task(val gen: Int, val scope: CallScope, val run: (CallScope) -> Unit)
    private class Report(val gen: Int, val url: String, val json: String, val scope: CallScope)

    private val lock = Object()
    private val control = ArrayDeque<Task>()
    private var report: Report? = null
    private var scope = CallScope()
    private var disposed = false
    val rejected = AtomicInteger(0)

    private val thread = Thread({ loop() }, threadName).apply { isDaemon = true; start() }

    /** Drops everything queued, empties the report slot and cancels the calls in flight. */
    fun cancelAll() {
        val old: CallScope
        synchronized(lock) {
            old = scope
            scope = CallScope()
            control.clear()
            report = null
        }
        old.cancel()
    }

    /** Queues control work for generation [gen]; false when the queue is full or the worker is disposed. */
    fun run(gen: Int, task: (CallScope) -> Unit): Boolean = synchronized(lock) {
        if (disposed) return false
        if (control.size >= queueCapacity) {
            rejected.incrementAndGet()
            return false
        }
        control.addLast(Task(gen, scope, task))
        lock.notifyAll()
        true
    }

    /** Puts [json] in the report slot (replacing a pending one); posted when the worker has no control work. */
    fun report(gen: Int, url: String, json: String) {
        synchronized(lock) {
            if (disposed) return
            report = Report(gen, url, json, scope)
            lock.notifyAll()
        }
    }

    private fun loop() {
        while (true) {
            var task: Task? = null
            var rep: Report? = null
            synchronized(lock) {
                while (!disposed && control.isEmpty() && report == null) lock.wait()
                if (disposed) return
                task = control.removeFirstOrNull()
                if (task == null) {
                    rep = report
                    report = null
                }
            }
            val t = task
            val r = rep
            try {
                if (t != null) {
                    if (gate.isCurrent(t.gen) && !t.scope.cancelled) t.run(t.scope)
                } else if (r != null && gate.isCurrent(r.gen) && !r.scope.cancelled) {
                    http.post(r.url, r.json, r.scope)
                }
            } catch (e: Throwable) {
                // one failed task must not end the worker
            }
        }
    }

    fun dispose() {
        val s: CallScope
        synchronized(lock) {
            if (disposed) return
            disposed = true
            s = scope
            control.clear()
            report = null
            lock.notifyAll()
        }
        s.cancel()
        thread.interrupt()
    }

    val isShutdown: Boolean get() = synchronized(lock) { disposed } && !thread.isAlive

    /** Test aid: waits for the worker thread to end after [dispose]. */
    fun awaitTermination(ms: Long): Boolean {
        thread.join(ms)
        return !thread.isAlive
    }

    val queued: Int get() = synchronized(lock) { control.size + (if (report != null) 1 else 0) }
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
