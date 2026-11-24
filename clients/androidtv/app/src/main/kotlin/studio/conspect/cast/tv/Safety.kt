package studio.conspect.cast.tv

import androidx.media3.common.ParserException
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.HttpDataSource
import androidx.media3.datasource.TransferListener
import androidx.media3.exoplayer.hls.playlist.HlsPlaylist
import androidx.media3.exoplayer.hls.playlist.HlsPlaylistParserFactory
import androidx.media3.exoplayer.hls.playlist.HlsMultivariantPlaylist
import androidx.media3.exoplayer.hls.playlist.HlsMediaPlaylist
import androidx.media3.exoplayer.upstream.ParsingLoadable
import java.io.IOException
import java.io.InputStream

/** Removes the cast token from any text before it reaches a log or an exception (ADR-0114 decision 5.8). */
@UnstableApi
object Redact {
    private val PATH = Regex("/cast/[A-Za-z0-9_-]{20,128}")
    private val BARE = Regex("[A-Za-z0-9_-]{43}")

    fun text(s: String?): String =
        if (s == null) "" else BARE.replace(PATH.replace(s, "/cast/<redacted>"), "<redacted>")

    /** A copy of [e] without cause chain or URL-bearing message: class name and redacted message only. */
    fun io(e: IOException): IOException = IOException("${e.javaClass.simpleName}: ${text(e.message)}")

    /** Runs a playlist parse; a failure leaves with a redacted message and no cause. */
    fun <T> guardParse(f: () -> T): T = try {
        f()
    } catch (e: ParserException) {
        throw ParserException.createForMalformedManifest(text(e.message), null)
    } catch (e: IOException) {
        throw io(e)
    }

    /** HTTP status of a Media3 HTTP failure, or null. */
    fun statusOf(e: IOException): Int? = (e as? HttpDataSource.InvalidResponseCodeException)?.responseCode
}

/** True when an answer under the token means the token is gone (receiver.js `gone`). */
fun isTokenGone(status: Int): Boolean = status == 404 || status == 403 || status == 410

/** Wraps a data source so IOExceptions leave it redacted, reporting HTTP statuses first. */
@UnstableApi
class RedactingDataSource(
    private val upstream: DataSource,
    private val onHttpStatus: (Int) -> Unit,
) : DataSource {
    override fun addTransferListener(l: TransferListener) = upstream.addTransferListener(l)

    override fun open(dataSpec: DataSpec): Long = guard { upstream.open(dataSpec) }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int = guard { upstream.read(buffer, offset, length) }

    override fun getUri() = upstream.uri

    override fun getResponseHeaders() = upstream.responseHeaders

    override fun close() = guard { upstream.close() }

    private inline fun <T> guard(f: () -> T): T = try {
        f()
    } catch (e: IOException) {
        Redact.statusOf(e)?.let(onHttpStatus)
        throw Redact.io(e)
    }
}

/** Wraps a playlist parser so a malformed playlist's exception text carries no token. */
@UnstableApi
class RedactingParser<T : Any>(private val delegate: ParsingLoadable.Parser<T>) : ParsingLoadable.Parser<T> {
    override fun parse(uri: android.net.Uri, inputStream: InputStream): T = Redact.guardParse { delegate.parse(uri, inputStream) }
}

@UnstableApi
class RedactingParserFactory(private val inner: HlsPlaylistParserFactory) : HlsPlaylistParserFactory {
    override fun createPlaylistParser(): ParsingLoadable.Parser<HlsPlaylist> =
        RedactingParser(inner.createPlaylistParser())

    override fun createPlaylistParser(
        multivariantPlaylist: HlsMultivariantPlaylist,
        previousMediaPlaylist: HlsMediaPlaylist?,
    ): ParsingLoadable.Parser<HlsPlaylist> =
        RedactingParser(inner.createPlaylistParser(multivariantPlaylist, previousMediaPlaylist))
}

/** Rejects callbacks of an attempt that a newer LOAD, fallback or stop has replaced. */
class GenerationGate {
    @Volatile
    var current: Int = 0
        private set

    fun next(): Int {
        current += 1
        return current
    }

    fun invalidate() {
        current += 1
    }

    fun isCurrent(gen: Int): Boolean = gen == current
}

/** What to do about lost packets. Recovery stays inside the SRT connection: no reopen, so the one-time sid is not reused. */
enum class LossAction { CONTINUE_IN_CONNECTION, STEP_DOWN }

class LossPolicy(private val budget: FlushBudget = FlushBudget()) {
    /** The TS reader resynchronises at the next keyframe by itself (continuity discontinuity); only the budget decides. */
    fun onLoss(nowMs: Long): LossAction =
        if (budget.record(nowMs)) LossAction.STEP_DOWN else LossAction.CONTINUE_IN_CONNECTION
}

object LiveEdge {
    const val MAX_OFFSET_MS = 1000L

    /**
     * Position to seek to so the live offset returns to [targetMs], or null when no seek is needed or the target is
     * not already buffered: a seek outside the buffer would reopen the SRT source, which is never done.
     */
    fun seekTargetMs(positionMs: Long, bufferedPositionMs: Long, offsetMs: Long, targetMs: Long): Long? {
        if (offsetMs <= MAX_OFFSET_MS) return null
        val target = positionMs + (offsetMs - targetMs)
        return if (target <= bufferedPositionMs) target else null
    }
}

/** One periodic report loop; `start` replaces any earlier loop, `cancel` ends it. */
class ReportTicker(
    private val post: (delayMs: Long, task: Runnable) -> Unit,
    private val cancelTask: (Runnable) -> Unit,
    private val periodMs: Long,
    private val send: () -> Unit,
) {
    private var task: Runnable? = null

    fun start() {
        cancel()
        val t = object : Runnable {
            override fun run() {
                if (task !== this) return
                send()
                post(periodMs, this)
            }
        }
        task = t
        post(periodMs, t)
    }

    fun cancel() {
        task?.let(cancelTask)
        task = null
    }
}

/**
 * Frame liveness for the no-frame-for-2-s rule. Frames are signalled per rendered frame (from the playback thread),
 * the watchdog reads it on the main thread. Thread-safe.
 */
class FrameMonitor(private val stallMs: Long = 2000L) {
    @Volatile
    private var lastMs = -1L
    private val count = java.util.concurrent.atomic.AtomicLong(0)

    fun onFrame(nowMs: Long) {
        lastMs = nowMs
        count.incrementAndGet()
    }

    val frames: Long get() = count.get()
    val hasFrame: Boolean get() = lastMs >= 0

    /** True when a frame was seen before and none has been rendered for longer than the stall time. */
    fun stalled(nowMs: Long): Boolean = lastMs >= 0 && nowMs - lastMs > stallMs

    fun reset() {
        lastMs = -1L
        count.set(0)
    }
}
