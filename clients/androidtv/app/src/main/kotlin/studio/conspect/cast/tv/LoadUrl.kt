package studio.conspect.cast.tv

import java.net.URI

/** A validated Cast LOAD URL. [token] stays in memory only; [toString] never prints it. */
class ParsedLoad internal constructor(
    /** `https://host[:port]`, the engine origin. Safe to store and log. */
    val origin: String,
    internal val token: String,
) {
    /** `<origin>/cast/<token>/`, the base of the token routes. Contains the token: never log it. */
    internal val base: String get() = "$origin/cast/$token/"

    val masterUrl: String get() = base + "master.m3u8"

    override fun toString(): String = "ParsedLoad(origin=$origin, token=<redacted>)"
}

/**
 * The LOAD check, the same rule as `receiver.js` (ADR-0114 decision 7): https only, path
 * `/cast/<token>/master.m3u8` with a 43-character base64url token, no credentials, no query, no fragment.
 */
object LoadUrl {
    private val PATH = Regex("^/cast/([A-Za-z0-9_-]{43})/master\\.m3u8$")

    fun parse(url: String?): ParsedLoad? {
        if (url.isNullOrEmpty() || url.length > 512) return null
        val uri = try {
            URI(url)
        } catch (e: Exception) {
            return null
        }
        if (uri.scheme != "https") return null
        if (uri.rawUserInfo != null) return null
        if (uri.rawQuery != null || uri.rawFragment != null) return null
        if (url.contains('?') || url.contains('#')) return null
        val host = uri.host ?: return null
        val path = uri.rawPath ?: return null
        val match = PATH.matchEntire(path) ?: return null
        val port = if (uri.port == -1) "" else ":${uri.port}"
        val hostPart = if (host.contains(':') && !host.startsWith("[")) "[$host]" else host
        return ParsedLoad("https://$hostPart$port", match.groupValues[1])
    }
}
