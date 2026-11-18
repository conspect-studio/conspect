package studio.conspect.cast.tv

import android.content.Context
import com.google.android.gms.cast.MediaError
import com.google.android.gms.cast.MediaLoadRequestData
import com.google.android.gms.cast.tv.CastLaunchRequest
import com.google.android.gms.cast.tv.CastReceiverOptions
import com.google.android.gms.cast.tv.ReceiverOptionsProvider
import com.google.android.gms.cast.tv.media.MediaException
import com.google.android.gms.tasks.Task
import com.google.android.gms.tasks.Tasks

const val CAST_NAMESPACE = "urn:x-cast:studio.conspect.cast"

/** Named in the manifest (RECEIVER_OPTIONS_PROVIDER_CLASS_NAME). */
class CastReceiverOptionsProvider : ReceiverOptionsProvider {
    override fun getOptions(context: Context): CastReceiverOptions {
        val memory = FailureMemory(PrefsFailureStore(context.applicationContext), Clock { System.currentTimeMillis() })
        return CastReceiverOptions.Builder(context)
            .setCustomNamespaces(listOf(CAST_NAMESPACE))
            .setLaunchRequestChecker(DeclineWhileSrtFailed(memory))
            .build()
    }
}

/**
 * Declines a Cast launch while the SRT failure memory is active; the platform then loads the Web Receiver
 * (MoQ, WebRTC, HLS). A launch request carries no engine origin, so any active origin declines.
 */
class DeclineWhileSrtFailed(private val memory: FailureMemory) : CastReceiverOptions.LaunchRequestChecker {
    override fun checkLaunchRequestSupported(request: CastLaunchRequest): Task<Boolean> =
        Tasks.forResult(!memory.anyActive())
}

/** The LOAD data check; a rejected load answers LOAD_FAILED without echoing the URL. */
object LoadCheck {
    fun urlOf(request: MediaLoadRequestData): String? =
        request.mediaInfo?.contentUrl ?: request.mediaInfo?.contentId

    fun reject(): MediaException =
        MediaException(MediaError.Builder().setDetailedErrorCode(905).setReason("INVALID_REQUEST").build())
}
