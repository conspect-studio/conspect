package studio.conspect.cast.tv

import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/** base64url without padding, no platform dependency (java.util.Base64 needs API 26, the floor is 23). */
internal object B64Url {
    private const val ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

    fun encode(data: ByteArray): String {
        val sb = StringBuilder((data.size * 4 + 2) / 3)
        var i = 0
        while (i < data.size) {
            val b0 = data[i].toInt() and 0xff
            val b1 = if (i + 1 < data.size) data[i + 1].toInt() and 0xff else 0
            val b2 = if (i + 2 < data.size) data[i + 2].toInt() and 0xff else 0
            sb.append(ALPHABET[b0 shr 2])
            sb.append(ALPHABET[((b0 and 3) shl 4) or (b1 shr 4)])
            if (i + 1 < data.size) sb.append(ALPHABET[((b1 and 15) shl 2) or (b2 shr 6)])
            if (i + 2 < data.size) sb.append(ALPHABET[b2 and 63])
            i += 3
        }
        return sb.toString()
    }

    /** Null on any character outside the alphabet or an impossible length. */
    fun decode(text: String): ByteArray? {
        if (text.length % 4 == 1) return null
        val out = ArrayList<Byte>(text.length * 3 / 4)
        var acc = 0
        var bits = 0
        for (c in text) {
            val v = ALPHABET.indexOf(c)
            if (v < 0) return null
            acc = (acc shl 6) or v
            bits += 6
            if (bits >= 8) {
                bits -= 8
                out.add(((acc shr bits) and 0xff).toByte())
            }
        }
        return out.toByteArray()
    }
}

/** The SRT passphrase and stream id of ADR-0114 decision 5.7. */
object CastSrt {
    private const val SALT = "conspect-cast-srt-v2"

    private fun hmac(key: ByteArray, msg: ByteArray): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(key, "HmacSHA256"))
        return mac.doFinal(msg)
    }

    /**
     * `prk = HMAC-SHA256(key = "conspect-cast-srt-v2", msg = T)`;
     * `okm = HMAC-SHA256(key = prk, msg = "passphrase" || S || 0x01)` (HKDF-Expand, L = 32, one block);
     * passphrase = base64url-no-pad(okm), 43 characters. `T` is the decoded token, `S` the sid as given.
     * Returns null when the token does not decode to 32 bytes.
     */
    fun passphrase(token: String, sid: String): String? {
        val t = B64Url.decode(token) ?: return null
        if (t.size != 32) return null
        val prk = hmac(SALT.toByteArray(Charsets.US_ASCII), t)
        val info = "passphrase".toByteArray(Charsets.US_ASCII) + sid.toByteArray(Charsets.US_ASCII) + byteArrayOf(1)
        return B64Url.encode(hmac(prk, info))
    }

    /** SRT access control stream id, `#!::r=cast/<sid>,m=request`. */
    fun streamId(sid: String): String = "#!::r=cast/$sid,m=request"
}

/** libsrt version, as the `0xXXYYZZ` integer `srt_getversion` returns. */
object LibsrtVersion {
    /** 1.5.7: fixes GHSA-m96h-3rc9-xx6h; the app refuses to use anything older. */
    const val FLOOR: Int = 0x010507

    fun isAtLeastFloor(version: Int): Boolean = version >= FLOOR

    fun format(version: Int): String =
        "${(version shr 16) and 0xff}.${(version shr 8) and 0xff}.${version and 0xff}"
}
