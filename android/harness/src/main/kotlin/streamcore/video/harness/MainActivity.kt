package streamcore.video.harness

import android.app.Activity
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import streamcore.video.android.HandlerSerialQueue
import streamcore.video.android.MediaCodecDecoder
import streamcore.video.android.MtkProfile
import uniffi.streamcore_transport.ChannelPath
import uniffi.streamcore_transport.ChannelTransport
import uniffi.streamcore_transport.OperationCancellation
import uniffi.streamcore_transport.PeerIdentity
import uniffi.streamcore_transport.SecureChannel
import uniffi.streamcore_transport.SessionOptions
import uniffi.streamcore_transport.connectSecure
import uniffi.streamcore_transport.pairWithHost
import uniffi.streamcore_video.ClientEvents
import uniffi.streamcore_video.ClientJob
import uniffi.streamcore_video.ClientParts
import uniffi.streamcore_video.ClientStats
import uniffi.streamcore_video.ClientStream
import uniffi.streamcore_video.Codec
import uniffi.streamcore_video.ColorPrimaries
import uniffi.streamcore_video.ColorTags
import uniffi.streamcore_video.PresentMode
import uniffi.streamcore_video.StreamPort
import uniffi.streamcore_video.TransferFunction
import uniffi.streamcore_video.YCbCrMatrix

private const val TAG = "StreamCoreVideo"
private const val DEMO_CAPABILITY = "streamcore.video-demo.v1"
private const val FRAMES_V4 = "frames.v4"
private const val STREAM_ID = 1u

/**
 * Bring-up harness: paste `video_host`'s connect line
 * (`streamcore-video://<host>:<port>?pair=<port>&pin=<pin>&fp=<fingerprint>`), connect, and
 * watch the stream decode on a full-screen SurfaceView with the client's counters on top.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {
    private val main = Handler(Looper.getMainLooper())
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private lateinit var surfaceView: SurfaceView
    private lateinit var statsView: TextView
    private lateinit var line: EditText
    private lateinit var connect: Button
    private var surface: Surface? = null

    /** `--es mtk none|vpp|vilte|all` (default all): the MediaTek switches to test. */
    private var mtkProfile = MtkProfile.ALL
    private var session: Session? = null
    private var lastDecoded = 0uL
    private var lastStatsAt = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        surfaceView = SurfaceView(this).apply { holder.addCallback(this@MainActivity) }
        statsView = TextView(this).apply {
            setTextColor(Color.WHITE)
            setBackgroundColor(0x80000000.toInt())
            textSize = 12f
            setPadding(16, 16, 16, 16)
            typeface = android.graphics.Typeface.MONOSPACE
        }
        line = EditText(this).apply {
            hint = "streamcore-video://host:port?pair=…&pin=…&fp=…"
            setSingleLine()
            setTextColor(Color.WHITE)
            setHintTextColor(Color.GRAY)
        }
        connect = Button(this).apply {
            text = "Connect"
            setOnClickListener { toggle() }
        }
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(0x80000000.toInt())
            addView(line, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            addView(connect)
        }
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surfaceView, FrameLayout.LayoutParams(-1, -1))
            addView(statsView, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START))
            addView(bar, FrameLayout.LayoutParams(-1, -2, Gravity.BOTTOM))
        })
        fullScreen()
        preferTopRefreshRate()
        intent?.getStringExtra("connect")?.let { line.setText(it) }
        mtkProfile = MtkProfile.parse(intent?.getStringExtra("mtk"))
        Log.i(TAG, "MediaTek profile: ${mtkProfile.name.lowercase()}")
        main.post(::refreshStats)
    }

    override fun onDestroy() {
        disconnect()
        scope.cancel()
        main.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    // -----------------------------------------------------------------------
    // Display: full screen at the display's top refresh rate (Responsive).

    private fun fullScreen() {
        if (Build.VERSION.SDK_INT >= 30) {
            window.insetsController?.let {
                it.hide(WindowInsets.Type.systemBars())
                it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_FULLSCREEN or
                View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
        }
    }

    private fun topRefreshRate(): Float {
        @Suppress("DEPRECATION")
        val display = if (Build.VERSION.SDK_INT >= 30) display else windowManager.defaultDisplay
        return display?.supportedModes?.maxOfOrNull { it.refreshRate } ?: 60f
    }

    private fun preferTopRefreshRate() {
        @Suppress("DEPRECATION")
        val display = (if (Build.VERSION.SDK_INT >= 30) display else windowManager.defaultDisplay) ?: return
        val current = display.mode
        val best = display.supportedModes
            .filter { it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight }
            .maxByOrNull { it.refreshRate } ?: return
        window.attributes = window.attributes.apply { preferredDisplayModeId = best.modeId }
        Log.i(TAG, "display mode ${best.modeId}: ${best.physicalWidth}x${best.physicalHeight} @ ${best.refreshRate} Hz")
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surface = holder.surface
        if (Build.VERSION.SDK_INT >= 30) {
            holder.surface.setFrameRate(topRefreshRate(), Surface.FRAME_RATE_COMPATIBILITY_DEFAULT)
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        disconnect()
        surface = null
    }

    // -----------------------------------------------------------------------
    // Session.

    private class Session(
        val channel: SecureChannel,
        val stream: ClientStream,
        val queue: HandlerSerialQueue,
        val decoder: MediaCodecDecoder,
        val reader: Job,
    )

    private class ConnectLine(val stream: String, val pairing: String, val pin: String, val fingerprint: String)

    private fun parse(text: String): ConnectLine? {
        val rest = text.trim().removePrefix("streamcore-video://").takeIf { it != text.trim() } ?: return null
        val authority = rest.substringBefore('?')
        val query = rest.substringAfter('?', "").split('&').associate { it.substringBefore('=') to it.substringAfter('=', "") }
        val host = authority.substringBeforeLast(':')
        val pair = query["pair"] ?: return null
        return ConnectLine(authority, "$host:$pair", query["pin"] ?: return null, query["fp"] ?: return null)
    }

    private fun toggle() {
        if (session != null) {
            disconnect()
            return
        }
        val target = parse(line.text.toString())
        val surface = surface
        if (target == null || surface == null) {
            status(if (target == null) "not a connect line" else "no surface yet")
            return
        }
        connect.isEnabled = false
        status("pairing with ${target.pairing}…")
        scope.launch {
            try {
                start(target, surface)
            } catch (e: Exception) {
                Log.e(TAG, "connect failed", e)
                main.post { status("connect failed: $e"); connect.isEnabled = true }
            }
        }
    }

    private suspend fun start(target: ConnectLine, surface: Surface) {
        val identity = PeerIdentity.generate()
        val host = pairWithHost(listOf(target.pairing), identity, target.pin, target.fingerprint, OperationCancellation())
        Log.i(TAG, "paired with ${target.fingerprint}")
        val channel = connectSecure(
            identity,
            host,
            listOf(ChannelPath(target.stream, ChannelTransport.TLS_TCP, 0u)),
            SessionOptions(listOf(DEMO_CAPABILITY, FRAMES_V4), listOf(DEMO_CAPABILITY)),
            OperationCancellation(),
        )
        Log.i(TAG, "connected to ${channel.remoteAddress()}, frames.v4 ${channel.framesV4()}")
        val port = StreamPort()
        val queue = HandlerSerialQueue(port)
        val decoder = MediaCodecDecoder(surface, TAG, mtkProfile)
        val stream = ClientStream(
            ClientParts(
                streamId = STREAM_ID,
                codec = Codec.H264,
                color = ColorTags(ColorPrimaries.BT709, TransferFunction.BT709, YCbCrMatrix.BT709, false),
                mode = PresentMode.RESPONSIVE,
                port = port,
                queue = queue,
                decoder = decoder,
                events = Jobs(channel),
                channel = channel,
                timings = null,
            ),
        )
        // The receive loop: video stays in Rust; anything else is logged.
        val reader = scope.launch {
            try {
                while (true) {
                    val message = stream.nextMessage(channel)
                    Log.i(TAG, "message kind ${message.payload.firstOrNull()} (${message.payload.size} bytes)")
                }
            } catch (e: Exception) {
                Log.i(TAG, "receive loop ended: $e")
                main.post { if (session?.channel === channel) disconnect() }
            }
        }
        main.post {
            session = Session(channel, stream, queue, decoder, reader)
            connect.text = "Disconnect"
            connect.isEnabled = true
            lastDecoded = 0uL
        }
    }

    private fun disconnect() {
        val s = session ?: return
        session = null
        connect.text = "Connect"
        s.reader.cancel()
        s.stream.beginClose()
        s.channel.closeChannel()
        // Let the close drain on the stream's queue, then release the codec and the thread.
        main.postDelayed({
            s.decoder.close()
            s.queue.quit()
        }, 1_000)
        Log.i(TAG, "disconnected")
    }

    /** ClientEvents: post-only, so each job is handed to the main thread. */
    private inner class Jobs(private val channel: SecureChannel) : ClientEvents {
        override fun post(job: ClientJob) {
            main.post { handle(job) }
        }

        private fun handle(job: ClientJob) {
            when (job) {
                is ClientJob.RequestKeyframe -> {
                    Log.i(TAG, "keyframe request (${job.reason})")
                    // The demo's own control message (envelope kind 0x01).
                    val body = """{"type":"keyframe"}""".toByteArray()
                    val message = byteArrayOf(0x01, 0, 0, 0, body.size.toByte()) + body
                    scope.launch {
                        try {
                            channel.enqueueApplicationFrame(message)
                        } catch (e: Exception) {
                            Log.w(TAG, "keyframe request not sent: $e")
                        }
                    }
                }
                is ClientJob.Stats -> {}
                is ClientJob.Feedback -> Log.d(TAG, "feedback for the consumer (${job.message.size} bytes)")
                else -> Log.i(TAG, "client: $job")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Overlay.

    private fun refreshStats() {
        main.postDelayed(::refreshStats, 1_000)
        val s = session ?: return
        val stats = s.stream.stats()
        val now = System.nanoTime()
        val seconds = if (lastStatsAt == 0L) 1.0 else (now - lastStatsAt) / 1e9
        val fps = (stats.decoded - lastDecoded).toDouble() / seconds
        lastDecoded = stats.decoded
        lastStatsAt = now
        val latency = s.decoder.decodeLatencyMs()
        val text = format(stats, fps, latency, s.decoder)
        statsView.text = text
        Log.i(TAG, text.replace('\n', ' '))
    }

    private fun format(stats: ClientStats, fps: Double, latency: Pair<Double, Double>?, decoder: MediaCodecDecoder) =
        buildString {
            appendLine("decoder ${decoder.decoderName} (${decoder.profileName})")
            appendLine("depth seen ${decoder.measuredDepth()}, learned ${stats.learnedDepth}")
            appendLine("decoded %.1f fps, total %d, presented %d".format(fps, stats.decoded.toLong(), stats.presented.toLong()))
            appendLine("queue bound ${stats.queueBound}, deep sessions ${stats.deepDecoderSessions}")
            if (latency != null) appendLine("decode C2→C3 p50 %.1f ms, p95 %.1f ms".format(latency.first, latency.second))
            appendLine("flushed ${stats.flushed}, present drops ${stats.presentDrops}, waiting drops ${stats.waitingDropped}")
            appendLine("keyframe requests ${stats.keyframeRequests} (deferred ${stats.keyframeRequestsDeferred})")
            append("recreates ${stats.recreates}, stalls ${stats.stalls}, feedback ${stats.feedbackSent}")
        }

    private fun status(text: String) {
        statsView.text = text
        Log.i(TAG, text)
    }
}
