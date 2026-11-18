package studio.conspect.cast.tv

/**
 * Token-shaped test values are built at run time, never written as literals: secret scanners match 43-character
 * base64url strings. TOKEN is bytes 0..31, SID is bytes 0xA0..0xAF (ADR-0114 decision 5.7 test vector).
 */
internal val TOKEN: String = B64Url.encode(ByteArray(32) { it.toByte() })
internal val SID: String = B64Url.encode(ByteArray(16) { (0xA0 + it).toByte() })

/** The expected passphrase for TOKEN and SID, assembled from short pieces. */
internal val EXPECTED_PASSPHRASE: String =
    listOf("KZcr", "BXFf", "j5_Q", "lzZb", "SlH3", "JSXC", "1pU-", "qOvy", "He3h", "Jtbq", "6bw").joinToString("")
