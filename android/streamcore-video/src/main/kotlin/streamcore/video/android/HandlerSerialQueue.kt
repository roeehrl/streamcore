package streamcore.video.android

import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.os.SystemClock
import uniffi.streamcore_video.SerialQueue
import uniffi.streamcore_video.StreamPort

/**
 * A client stream's executor queue (docs/VIDEO-THREADING.md): one HandlerThread at
 * THREAD_PRIORITY_URGENT_DISPLAY. `wake` posts `port.run()`, `wakeAfter` posts
 * `port.runTimer(token)` at its deadline; neither blocks or runs inline. The same
 * Looper carries [MediaCodecDecoder]'s MediaCodec callbacks and frame-rendered
 * listener, so the decoder's state is touched on one thread.
 */
class HandlerSerialQueue(
    private val port: StreamPort,
    name: String = "streamcore-video",
) : SerialQueue {
    private val thread = HandlerThread(name, Process.THREAD_PRIORITY_URGENT_DISPLAY).apply { start() }
    val handler = Handler(thread.looper)

    override fun wake() {
        handler.post { port.run() }
    }

    override fun wakeAfter(delayNs: ULong, timer: ULong) {
        // Handler time is in milliseconds: round up so a timer never fires early.
        val delayMs = ((delayNs + 999_999uL) / 1_000_000uL).toLong()
        handler.postAtTime({ port.runTimer(timer) }, SystemClock.uptimeMillis() + delayMs)
    }

    /** Stop the thread once the stream is closed. */
    fun quit() {
        thread.quitSafely()
    }
}
