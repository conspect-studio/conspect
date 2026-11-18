package studio.conspect.cast.tv

import java.io.IOException
import java.io.InterruptedIOException

/** libsrt receive timed out (no data within SRTO_RCVTIMEO). Not an error: the reader loops. */
class SrtTimeout : Exception()

/** The part of an SRT socket the reader needs; a fake peer implements it in tests. */
interface SrtLink {
    /** Receives one message into [buf]; throws [SrtTimeout] on RCVTIMEO, IOException when the link is gone. */
    fun recv(buf: ByteArray): Int

    /** Closes the socket; unblocks a recv in another thread. */
    fun close()
}

/**
 * Reads SRT messages with a finite receive timeout, so a connected peer that sends nothing never pins the
 * loader thread: between timeouts it checks the cancel flag and thread interruption. [process] may return empty
 * (the loss filter swallowed the message); the reader then receives again. Holds one message at most.
 */
class SrtReader(
    private val link: SrtLink,
    private val process: (ByteArray, Int) -> ByteArray = { b, n -> b.copyOf(n) },
) {
    @Volatile
    var cancelled = false
        private set
    private val message = ByteArray(MESSAGE_MAX)
    private var serve = ByteArray(0)
    private var pos = 0

    /** Bytes read, or -1 at end of input (cancelled or closed). Throws IOException on a broken link. */
    fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        if (length == 0) return 0
        while (pos >= serve.size) {
            if (cancelled) return -1
            if (Thread.currentThread().isInterrupted) throw InterruptedIOException("interrupted")
            val n = try {
                link.recv(message)
            } catch (e: SrtTimeout) {
                continue
            } catch (e: IOException) {
                if (cancelled) return -1
                throw e
            }
            serve = process(message, n)
            pos = 0
        }
        val n = minOf(length, serve.size - pos)
        System.arraycopy(serve, pos, buffer, offset, n)
        pos += n
        return n
    }

    /** From any thread: ends the read loop and closes the socket. */
    fun cancel() {
        cancelled = true
        runCatching { link.close() }
    }

    companion object {
        /** An SRT live message is at most 1 456 bytes (payload size); 1 500 leaves room. */
        const val MESSAGE_MAX = 1500
        const val RCVTIMEO_MS = 500
    }
}
