package studio.conspect.cast.tv

import android.net.Uri
import androidx.media3.common.C
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.BaseDataSource
import androidx.media3.datasource.DataSpec
import io.github.thibaultbee.srtdroid.core.Srt
import io.github.thibaultbee.srtdroid.core.enums.ErrorType
import io.github.thibaultbee.srtdroid.core.enums.SockOpt
import io.github.thibaultbee.srtdroid.core.enums.Transtype
import io.github.thibaultbee.srtdroid.core.models.SrtError
import io.github.thibaultbee.srtdroid.core.models.SrtSocket
import java.io.IOException
import java.net.InetSocketAddress
import java.net.SocketException
import java.net.SocketTimeoutException

/** Everything one SRT attempt needs. [passphrase] is a secret: [toString] never prints it. */
class SrtConfig(
    val host: String,
    val port: Int,
    val latencyMs: Int,
    val sid: String,
    private val passphrase: String,
) {
    internal fun passphrase(): String = passphrase
    override fun toString(): String = "SrtConfig(host=$host, port=$port, latencyMs=$latencyMs)"

    companion object {
        const val RCVBUF_BYTES = 2 * 1024 * 1024
        const val CONNECT_TIMEOUT_MS = 1500
        const val PBKEYLEN = 32
    }
}

/** Version of the libsrt that srtdroid loaded, or null if it could not be loaded. */
object SrtLibrary {
    fun versionOrNull(): Int? = try {
        Srt.startUp()
        Srt.version
    } catch (e: Throwable) {
        null
    }
}

/** The srtdroid socket as an [SrtLink]. */
private class SocketLink(private val s: SrtSocket) : SrtLink {
    override fun recv(buf: ByteArray): Int = try {
        s.recv(buf, 0, buf.size)
    } catch (e: SocketTimeoutException) {
        throw IOException("srt closed") // srtdroid maps a 0-byte receive (closed socket) to this
    } catch (e: SocketException) {
        if (SrtError.lastError == ErrorType.ETIMEOUT) throw SrtTimeout()
        throw IOException("srt read failed")
    }

    override fun close() {
        runCatching { s.close() }
    }
}

/**
 * Reads SRT payload (MPEG-TS) straight into the player's buffer: no queue of the app between libsrt and the
 * player (ADR-0114 "Receiver buffers"). The reader holds at most one message. libsrt has a 500 ms receive timeout,
 * so a silent peer never pins the loader thread: between timeouts the reader checks cancel and interruption, and
 * [close] closes the socket.
 *
 * Loss (libsrt's receive-drop counter, or a continuity gap of the video packets) starts a discard-to-IDR in the
 * [TsIdrFilter]: the connection and the one-time sid are kept, video is dropped until the next IDR access unit,
 * and [Events.onLoss] is told once per recovery.
 */
@UnstableApi
class SrtDataSource(
    private val config: SrtConfig,
    private val events: Events,
) : BaseDataSource(/* isNetwork = */ true) {

    interface Events {
        /** Connect failed or timed out (before any payload). */
        fun onConnectFailed()

        /** Packets were lost; the source discards to the next IDR by itself. Counts toward the step-down budget. */
        fun onLoss()
    }

    private var socket: SrtSocket? = null
    private var reader: SrtReader? = null
    private val filter = TsIdrFilter()
    private var lastDrops = 0
    private var lastStatsAtMs = 0L

    override fun open(dataSpec: DataSpec): Long {
        transferInitializing(dataSpec)
        val s = SrtSocket()
        try {
            s.setSockFlag(SockOpt.TRANSTYPE, Transtype.LIVE)
            s.setSockFlag(SockOpt.STREAMID, CastSrt.streamId(config.sid))
            s.setSockFlag(SockOpt.LATENCY, config.latencyMs)
            s.setSockFlag(SockOpt.RCVLATENCY, config.latencyMs)
            s.setSockFlag(SockOpt.RCVBUF, SrtConfig.RCVBUF_BYTES)
            s.setSockFlag(SockOpt.CONNTIMEO, SrtConfig.CONNECT_TIMEOUT_MS)
            s.setSockFlag(SockOpt.RCVTIMEO, SrtReader.RCVTIMEO_MS)
            // The passphrase is a socket option, never a URL.
            s.setSockFlag(SockOpt.PBKEYLEN, SrtConfig.PBKEYLEN)
            s.setSockFlag(SockOpt.PASSPHRASE, config.passphrase())
            s.setSockFlag(SockOpt.ENFORCEDENCRYPTION, true)
            s.connect(InetSocketAddress(config.host, config.port))
        } catch (e: Exception) {
            runCatching { s.close() }
            events.onConnectFailed()
            // Class name only: the cause text could carry the address.
            throw IOException("srt connect failed: ${e.javaClass.simpleName}")
        }
        socket = s
        filter.reset()
        lastDrops = 0
        lastStatsAtMs = 0
        reader = SrtReader(SocketLink(s)) { buf, n ->
            val before = filter.recoveries
            val out = filter.process(buf, 0, n)
            if (filter.recoveries != before) events.onLoss()
            checkDrops(s)
            out
        }
        transferStarted(dataSpec)
        return C.LENGTH_UNSET.toLong()
    }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        val r = reader ?: throw IOException("srt not open")
        val n = r.read(buffer, offset, length)
        if (n > 0) bytesTransferred(n)
        return if (n < 0) C.RESULT_END_OF_INPUT else n
    }

    private fun checkDrops(s: SrtSocket) {
        val now = android.os.SystemClock.elapsedRealtime()
        if (now - lastStatsAtMs < STATS_EVERY_MS) return
        lastStatsAtMs = now
        try {
            val drops = s.bistats(false, true).pktRcvDropTotal
            if (drops > lastDrops && filter.resync()) events.onLoss() // merged into a recovery already counted
            lastDrops = drops
        } catch (e: Exception) {
            // statistics are advisory
        }
    }

    override fun getUri(): Uri = Uri.parse("srt://cast")

    override fun close() {
        val r = reader
        reader = null
        socket = null
        try {
            r?.cancel()
        } finally {
            if (r != null) transferEnded()
        }
    }

    companion object {
        const val STATS_EVERY_MS = 250L
    }
}
