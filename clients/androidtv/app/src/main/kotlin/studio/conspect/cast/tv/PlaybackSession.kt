package studio.conspect.cast.tv

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DataSource
import androidx.media3.exoplayer.DefaultLoadControl
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.analytics.AnalyticsListener
import androidx.media3.exoplayer.video.VideoFrameMetadataListener
import androidx.media3.exoplayer.hls.HlsMediaSource
import androidx.media3.exoplayer.hls.playlist.DefaultHlsPlaylistParserFactory
import androidx.media3.exoplayer.source.MediaSource
import androidx.media3.exoplayer.source.ProgressiveMediaSource
import androidx.media3.exoplayer.upstream.DefaultLoadErrorHandlingPolicy
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.common.util.TimestampAdjuster
import androidx.media3.extractor.Extractor
import androidx.media3.extractor.ExtractorsFactory
import androidx.media3.extractor.ts.DefaultTsPayloadReaderFactory
import androidx.media3.extractor.ts.TsExtractor

/** Where a session's player lives: the activity gives the player a surface through [PlayerHost]. */
interface PlayerHost {
    fun attach(player: ExoPlayer)
    fun detach()
}

/**
 * One cast: ladder call, SRT attempt with its 2.0 s deadline, HLS as the floor, live step-down, reports.
 * The token is held here in memory only. Threading: network on [io], player on the main thread.
 */
@UnstableApi
class PlaybackSession(
    private val context: Context,
    private val host: PlayerHost,
    private val receiver: ReceiverInfo,
    private val memory: FailureMemory,
    http: Http = UrlHttp,
    private val clock: Clock = Clock { SystemClock.elapsedRealtime() },
    private val onState: (String) -> Unit,
    private val onReportJson: (String) -> Unit,
    private val onEnded: () -> Unit,
) {

    private val main = Handler(Looper.getMainLooper())
    private var load: ParsedLoad? = null
    private val gate = GenerationGate()
    private val net = NetWorker(gate, http)
    private val httpClient = http
    private val tokenCheck = TokenCheck()
    private val frames = FrameMonitor(STALL_MS)
    private var disposed = false
    private val generation: Int get() = gate.current
    private var player: ExoPlayer? = null
    private var rung: String? = null
    private var attemptStartMs = 0L
    private var firstFrameMs: Long? = null
    private var stalls = 0
    private var softResets = 0
    private var lastError: String? = null
    private var state = "loading"

    private val ticker = ReportTicker(
        post = { d, r -> main.postDelayed(r, d) },
        cancelTask = { main.removeCallbacks(it) },
        periodMs = REPORT_EVERY_MS,
        send = { sendReport() },
    )
    private val lossPolicy get() = losses
    private var losses = LossPolicy()

    val player0: ExoPlayer? get() = player

    fun start(parsed: ParsedLoad) {
        if (disposed) return
        stop()
        val gen = gate.next()
        load = parsed
        rung = null
        firstFrameMs = null
        frames.reset()
        tokenCheck.reset()
        stalls = 0
        softResets = 0
        lastError = null
        losses = LossPolicy()
        setState("loading")
        net.run(gen) { negotiate(parsed, gen, it) }
    }

    /** Ends the session for good (Activity destroyed): stops everything, shuts the network worker down. */
    fun dispose() {
        if (disposed) return
        stop()
        disposed = true
        net.dispose()
    }

    fun stop() {
        gate.invalidate()
        net.cancelAll()
        tokenCheck.reset()
        ticker.cancel()
        main.removeCallbacksAndMessages(null)
        releasePlayer()
        load = null
    }

    // ---- background: master.m3u8, then the ladder ----

    private fun negotiate(p: ParsedLoad, gen: Int, scope: CallScope) {
        val master = httpClient.get(p.masterUrl, scope)
        if (isTokenGone(master.status)) {
            main.post { if (gen == generation) end("ended") }
            return
        }
        val libsrt = SrtLibrary.versionOrNull()
        val srtOk = libsrt != null && LibsrtVersion.isAtLeastFloor(libsrt)
        val answer = ladder(p, if (srtOk) LibsrtVersion.format(libsrt!!) else null, null, scope)
        main.post {
            if (gen != generation) return@post
            when (answer) {
                is LadderAnswer.Srt -> if (srtOk) playSrt(p, answer, gen) else playHls(p, gen)
                is LadderAnswer.NotFound -> playHls(p, gen) // master answered, so an engine without the ladder
                else -> playHls(p, gen)
            }
        }
    }

    /** One ladder call; on 429 one retry after Retry-After (R2-N2), else the caller steps down to HLS. */
    private fun ladder(p: ParsedLoad, libsrt: String?, failed: LadderFailure?, scope: CallScope): LadderAnswer {
        val skip = memory.secondsLeft(p.origin)
        val body = LadderRequest.body(receiver, libsrt, skip, failed)
        var r = httpClient.post(p.base + "ladder", body, scope)
        var a = LadderParser.parse(r.status, r.body, r.retryAfter)
        if (a is LadderAnswer.RateLimited) {
            if (!scope.sleep(a.retryAfterSeconds * 1000L)) return LadderAnswer.Unusable
            r = httpClient.post(p.base + "ladder", body, scope)
            a = LadderParser.parse(r.status, r.body, r.retryAfter)
            if (a is LadderAnswer.RateLimited) a = LadderAnswer.Hls
        }
        return a
    }

    // ---- player ----

    private fun newPlayer(): ExoPlayer {
        val lc = DefaultLoadControl.Builder()
            .setBufferDurationsMs(MIN_BUFFER_MS, MAX_BUFFER_MS, PLAYBACK_START_MS, PLAYBACK_START_MS)
            .setTargetBufferBytes(16 * 1024 * 1024)
            .setPrioritizeTimeOverSizeThresholds(false)
            .setBackBuffer(0, false)
            .build()
        val p = ExoPlayer.Builder(context).setLoadControl(lc).build()
        val gen = gate.current
        // Per rendered frame, on the playback thread; a frame of a replaced attempt is ignored.
        p.setVideoFrameMetadataListener(
            VideoFrameMetadataListener { _, _, _, _ -> if (gate.isCurrent(gen)) frames.onFrame(clock.nowMs()) },
        )
        p.addAnalyticsListener(object : AnalyticsListener {
            override fun onRenderedFirstFrame(
                eventTime: AnalyticsListener.EventTime,
                output: Any,
                renderTimeMs: Long,
            ) {
                if (gate.isCurrent(gen)) onFrame()
            }
        })
        p.addListener(object : Player.Listener {
            override fun onPlayerError(error: PlaybackException) {
                // Class and code only: messages of HTTP errors carry the URL, which holds the token.
                lastError = "player error ${error.errorCodeName}"
                stepDown("transport closed")
            }

            override fun onPlaybackStateChanged(playbackState: Int) {
                if (playbackState == Player.STATE_BUFFERING && firstFrameMs != null) stalls += 1
                if (playbackState == Player.STATE_ENDED && rung == "srt") stepDown("transport closed")
                setState(
                    when (playbackState) {
                        Player.STATE_READY -> "playing"
                        Player.STATE_BUFFERING -> "buffering"
                        Player.STATE_ENDED -> "idle"
                        else -> state
                    },
                )
            }
        })
        host.attach(p)
        player = p
        return p
    }

    private fun releasePlayer() {
        player?.let {
            host.detach()
            it.release()
        }
        player = null
    }

    private fun playSrt(p: ParsedLoad, a: LadderAnswer.Srt, gen: Int) {
        val pass = CastSrt.passphrase(p.token, a.sid)
        if (pass == null) {
            srtFailed("connect", gen)
            return
        }
        rung = "srt"
        attemptStartMs = clock.nowMs()
        val cfg = SrtConfig(a.host, a.port, a.latencyMs, a.sid, pass)
        val extractors = ExtractorsFactory {
            arrayOf<Extractor>(
                TsExtractor(
                    TsExtractor.MODE_SINGLE_PMT,
                    TimestampAdjuster(TimestampAdjuster.MODE_NO_OFFSET),
                    DefaultTsPayloadReaderFactory(0),
                ),
            )
        }
        val source: MediaSource = ProgressiveMediaSource.Factory(
            DataSource.Factory { SrtDataSource(cfg, srtEvents(gen)) },
            extractors,
        ).setLoadErrorHandlingPolicy(DefaultLoadErrorHandlingPolicy(1)).createMediaSource(liveItem("srt://cast"))
        val pl = newPlayer()
        pl.setPlaybackParameters(androidx.media3.common.PlaybackParameters(1f))
        pl.setMediaSource(source)
        pl.playWhenReady = true
        pl.prepare()
        // Deadline: 2.0 s from the start of the attempt to the first rendered frame.
        main.postDelayed({ if (gen == generation && firstFrameMs == null && rung == "srt") srtFailed("first-frame", gen) },
            SRT_DEADLINE_MS)
        scheduleWatchdog(gen)
        scheduleReport(gen)
    }

    private fun playHls(p: ParsedLoad, gen: Int) {
        rung = "hls"
        firstFrameMs = null
        attemptStartMs = clock.nowMs()
        releasePlayer()
        val pl = newPlayer()
        val http = DefaultHttpDataSource.Factory()
        val source = HlsMediaSource.Factory(
            DataSource.Factory { RedactingDataSource(http.createDataSource()) { st -> if (isTokenGone(st)) checkToken(gen) } },
        ).setPlaylistParserFactory(RedactingParserFactory(DefaultHlsPlaylistParserFactory()))
            .createMediaSource(MediaItem.fromUri(p.masterUrl))
        pl.setMediaSource(source)
        pl.playWhenReady = true
        pl.prepare()
        scheduleReport(gen)
    }

    private fun liveItem(uri: String): MediaItem = MediaItem.Builder()
        .setUri(uri)
        .setLiveConfiguration(
            MediaItem.LiveConfiguration.Builder()
                .setTargetOffsetMs(LIVE_TARGET_OFFSET_MS)
                .setMinPlaybackSpeed(0.97f)
                .setMaxPlaybackSpeed(1.03f)
                .build(),
        ).build()

    private fun onFrame() {
        val now = clock.nowMs()
        frames.onFrame(now)
        if (firstFrameMs == null) {
            firstFrameMs = now - attemptStartMs
            setState("playing")
            sendReport()
        }
    }

    // ---- fallback ----

    /** SRT connect failure / no first frame in 2.0 s: report to the ladder, remember 15 min, play HLS. */
    private fun srtFailed(stage: String, gen: Int) {
        val p = load ?: return
        if (gen != generation || rung != "srt") return
        val ms = clock.nowMs() - attemptStartMs
        memory.remember(p.origin)
        releasePlayer()
        playHls(p, gen)
        net.run(gen) { ladder(p, null, LadderFailure(stage, ms), it) }
    }

    /** Live step-down on the playing SRT rung: no frame 2 s, transport closed, more than 3 flushes in 60 s. */
    private fun stepDown(why: String) {
        val p = load ?: return
        if (rung != "srt") {
            if (why == "transport closed") setState("error")
            return
        }
        val gen = generation
        if (firstFrameMs == null) {
            srtFailed("connect", gen)
            return
        }
        memory.remember(p.origin)
        val ms = clock.nowMs() - attemptStartMs
        releasePlayer()
        playHls(p, gen)
        net.run(gen) { ladder(p, null, LadderFailure("stall", ms), it) }
    }

    private fun scheduleWatchdog(gen: Int) {
        main.postDelayed({
            if (gen != generation || rung != "srt") return@postDelayed
            if (firstFrameMs != null && frames.stalled(clock.nowMs())) {
                stepDown("no frame for 2 s")
            } else {
                scheduleWatchdog(gen)
            }
        }, 500)
    }

    /** Callbacks of one SRT attempt; each is dropped when a newer attempt or LOAD has replaced it. */
    private fun srtEvents(gen: Int) = object : SrtDataSource.Events {
        override fun onConnectFailed() {
            main.post { if (gate.isCurrent(gen)) srtFailed("connect", gen) }
        }

        override fun onLoss() {
            main.post { if (gate.isCurrent(gen)) handleLoss() }
        }
    }

    /**
     * Recovery stays inside the connection: the TS reader drops the partial access unit at the gap and the sample
     * queue waits for the next keyframe. No seek reopens the source (the one-time sid is spent). Only a live offset
     * above 1 s that is already buffered is corrected by a seek. More than 3 losses in 60 s: step down.
     */
    private fun handleLoss() {
        val pl = player ?: return
        if (rung != "srt") return
        softResets += 1
        if (lossPolicy.onLoss(clock.nowMs()) == LossAction.STEP_DOWN) {
            stepDown("flushes")
            return
        }
        val offset = pl.currentLiveOffset
        if (offset != C.TIME_UNSET) {
            LiveEdge.seekTargetMs(pl.currentPosition, pl.bufferedPosition, offset, LIVE_TARGET_OFFSET_MS)
                ?.let { pl.seekTo(it) }
        }
    }

    // ---- reports ----

    private fun scheduleReport(@Suppress("UNUSED_PARAMETER") gen: Int) = ticker.start()

    private fun sendReport() {
        val p = load ?: return
        val json = Report.build(
            receiver, state, rung, firstFrameMs, frames.frames, null, stalls, softResets, lastError,
            clock.nowMs(),
        )
        onReportJson(json)
        net.report(gate.current, p.base + "report", json)
    }

    private fun setState(s: String) {
        state = s
        onState(s)
    }

    /** Something answered 404/403/410 under the token: ask master.m3u8 once; if it is gone too, end the cast. */
    private fun checkToken(gen: Int) {
        // Called from a loader thread: all state is touched on the main thread.
        main.post {
            val p = load ?: return@post
            if (!gate.isCurrent(gen) || !tokenCheck.begin(gen)) return@post
            val queued = net.run(gen) { scope ->
                val st = httpClient.get(p.masterUrl, scope).status
                main.post {
                    tokenCheck.finish(gen)
                    if (gate.isCurrent(gen) && isTokenGone(st)) end("ended")
                }
            }
            if (!queued) tokenCheck.finish(gen)
        }
    }

    /** The cast ended (revoked or expired token): final report, release the player, stop reports, drop the token. */
    private fun end(kind: String) {
        lastError = kind
        setState("error")
        sendReport()
        gate.invalidate()
        net.cancelAll()
        tokenCheck.reset()
        ticker.cancel()
        main.removeCallbacksAndMessages(null)
        releasePlayer()
        load = null
        onEnded()
    }

    companion object {
        const val SRT_DEADLINE_MS = 2000L
        const val STALL_MS = 2000L
        const val REPORT_EVERY_MS = 5000L
        const val LIVE_TARGET_OFFSET_MS = 100L
        const val MIN_BUFFER_MS = 50
        const val MAX_BUFFER_MS = 2000
        const val PLAYBACK_START_MS = 50
    }
}
