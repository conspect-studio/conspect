package studio.conspect.cast.tv

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.TextView
import android.support.v4.media.session.MediaSessionCompat
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.MediaSession
import androidx.media3.ui.PlayerView
import com.google.android.gms.cast.MediaLoadRequestData
import com.google.android.gms.cast.tv.CastReceiverContext
import com.google.android.gms.cast.tv.media.MediaLoadCommandCallback
import com.google.android.gms.tasks.Task
import com.google.android.gms.tasks.Tasks

/** Idle screen when opened by hand; the Cast Connect receiver and player surface when launched by a cast. */
@UnstableApi
class MainActivity : Activity(), PlayerHost {
    private lateinit var playerView: PlayerView
    private lateinit var idle: TextView
    private var session: PlaybackSession? = null
    private var mediaSession: MediaSession? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        playerView = PlayerView(this).apply { useController = false; visibility = android.view.View.GONE }
        idle = TextView(this).apply {
            text = "Cast from Conspect to this TV\nVersion ${BuildConfig.VERSION_NAME}"
            setTextColor(Color.WHITE)
            textSize = 28f
            gravity = Gravity.CENTER
        }
        val mp = ViewGroup.LayoutParams.MATCH_PARENT
        root.addView(playerView, FrameLayout.LayoutParams(mp, mp))
        root.addView(idle, FrameLayout.LayoutParams(mp, mp))
        setContentView(root)

        val ctx = CastReceiverContext.getInstance()
        val memory = FailureMemory(PrefsFailureStore(applicationContext), Clock { System.currentTimeMillis() })
        session = PlaybackSession(
            context = applicationContext,
            host = this,
            receiver = ReceiverInfo(BuildConfig.VERSION_NAME, Build.VERSION.SDK_INT),
            memory = memory,
            onState = { /* media status comes from the MediaSession */ },
            onReportJson = { json -> runCatching { ctx.sendMessage(CAST_NAMESPACE, null, json) } },
            onEnded = { runOnUiThread { showIdle() } },
        )
        ctx.mediaManager.setMediaLoadCommandCallback(object : MediaLoadCommandCallback() {
            override fun onLoad(senderId: String?, request: MediaLoadRequestData): Task<MediaLoadRequestData> {
                val parsed = LoadUrl.parse(LoadCheck.urlOf(request))
                    ?: return Tasks.forException(LoadCheck.reject())
                runOnUiThread { session?.start(parsed) }
                return Tasks.forResult(request)
            }
        })
        handle(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handle(intent)
    }

    private fun handle(intent: Intent?) {
        if (intent != null) CastReceiverContext.getInstance().mediaManager.onNewIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        CastReceiverContext.getInstance().start()
    }

    override fun onDestroy() {
        session?.dispose() // one network worker per Activity instance: shut down with it
        session = null
        super.onDestroy()
    }

    override fun onStop() {
        session?.stop()
        showIdle()
        CastReceiverContext.getInstance().stop()
        super.onStop()
    }

    private fun showIdle() {
        playerView.visibility = android.view.View.GONE
        idle.visibility = android.view.View.VISIBLE
    }

    override fun attach(player: ExoPlayer) {
        runOnUiThread {
            playerView.player = player
            playerView.visibility = android.view.View.VISIBLE
            idle.visibility = android.view.View.GONE
            mediaSession?.release()
            // Media status comes from this session; no title or URL (the URL holds the token).
            val ms = MediaSession.Builder(this, player).build()
            mediaSession = ms
            CastReceiverContext.getInstance().mediaManager.setSessionCompatToken(
                MediaSessionCompat.Token.fromToken(ms.platformToken),
            )
        }
    }

    override fun detach() {
        runOnUiThread {
            CastReceiverContext.getInstance().mediaManager.setSessionCompatToken(null)
            mediaSession?.release()
            mediaSession = null
            playerView.player = null
        }
    }
}
