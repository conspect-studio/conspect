package studio.conspect.cast.tv

import android.app.Application
import androidx.media3.common.util.Log
import androidx.media3.common.util.UnstableApi
import com.google.android.gms.cast.tv.CastReceiverContext

@UnstableApi
class ConspectApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Media3 logs failing URLs and stack traces; a URL here holds the cast token (ADR-0114 5.8).
        Log.setLogLevel(Log.LOG_LEVEL_OFF)
        Log.setLogStackTraces(false)
        CastReceiverContext.initInstance(this)
    }
}
