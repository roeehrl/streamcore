package streamcore.video.android

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Surface
import java.util.ArrayDeque
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import uniffi.streamcore_video.Codec
import uniffi.streamcore_video.ColorPrimaries
import uniffi.streamcore_video.ColorTags
import uniffi.streamcore_video.DecodeResult
import uniffi.streamcore_video.DecoderEvents
import uniffi.streamcore_video.DecoderInfo
import uniffi.streamcore_video.InvalidateOutcome
import uniffi.streamcore_video.MediaException
import uniffi.streamcore_video.PresentMode
import uniffi.streamcore_video.SampleMeta
import uniffi.streamcore_video.SerialQueue
import uniffi.streamcore_video.StreamDecoder
import uniffi.streamcore_video.TransferFunction
import uniffi.streamcore_video.YCbCrMatrix

/**
 * StreamCore's `StreamDecoder` over an asynchronous MediaCodec rendering to [surface]
 * (STREAMCORE-VIDEO.md 5). It only decodes and presents; ClientStream owns every policy.
 *
 * - Every call arrives on the stream's executor. With a [HandlerSerialQueue], MediaCodec's
 *   callbacks and the frame-rendered listener run on the same Looper; [DecoderEvents] calls
 *   only post, so calling them from those callbacks is safe.
 * - Decoders are tried hardware first, those with FEATURE_LowLatency first among them. Each
 *   is configured with KEY_LOW_LATENCY and the vendor low-latency keys, then with fewer keys
 *   when configure fails.
 * - A sample's `presentationTimeUs` is its wire sequence, so outputs and renders name it.
 * - Each output is released to the surface as soon as it arrives (Responsive): the
 *   compositor shows the newest per vsync, and a replaced frame gets no render callback,
 *   which ClientStream counts as a present drop. Smooth releases with a render time.
 * - C3 is the output callback's System.nanoTime(); C5 is the frame-rendered time (exact,
 *   `estimated = false`). [nowNs] is System.nanoTime().
 * - No free input buffer: `Busy`, then `inputReady` from the next onInputBufferAvailable.
 * - `flush` runs MediaCodec.flush() and start() on a helper thread and waits at most 450 ms.
 */
class MediaCodecDecoder(
    private val surface: Surface,
    private val tag: String = "StreamCoreVideo",
    /** The MediaTek switches to set on a MediaTek decoder (A/B tests). */
    private val mtkProfile: MtkProfile = MtkProfile.ALL,
) : StreamDecoder {
    private val lock = Any()
    private var codec: MediaCodec? = null
    private var events: DecoderEvents? = null
    /** Free input indexes, each with the codec epoch it was announced in (epoch << 32 | index). */
    private val freeInputs = ArrayDeque<Long>()

    /** Bumped by every flush and configure: an index from an older epoch is not used. */
    private var epoch = 0

    private fun entry(index: Int): Long = (epoch.toLong() shl 32) or (index.toLong() and 0xffffffffL)
    private var busy = false
    private var mode = PresentMode.RESPONSIVE

    /** Input submit times by sequence, for the decode latency (C2 → C3). */
    private val submitted = HashMap<Long, Long>()
    private val decodeNs = LongArray(240)
    private var decodeCount = 0
    private var inputsBeforeFirstOutput = 0
    private var firstOutput = false

    /** The key set or MediaTek profile in use, for diagnostics. */
    @Volatile
    var profileName: String = ""
        private set

    /** The name of the decoder in use, for diagnostics. */
    @Volatile
    var decoderName: String = ""
        private set

    private val helper = Executors.newSingleThreadExecutor { r -> Thread(r, "streamcore-video-flush") }

    override fun configure(
        codec: Codec,
        config: ByteArray,
        color: ColorTags,
        queue: SerialQueue,
        events: DecoderEvents,
    ): DecoderInfo {
        releaseCodec()
        val mime = when (codec) {
            Codec.H264 -> MediaFormat.MIMETYPE_VIDEO_AVC
            Codec.HEVC -> MediaFormat.MIMETYPE_VIDEO_HEVC
            Codec.AV1 -> throw MediaException.Unsupported()
        }
        val parameters = when (codec) {
            Codec.H264 -> ParameterSets.fromAvcC(config)
            else -> ParameterSets.fromHvcC(config)
        } ?: throw MediaException.InvalidSettings()
        val handler = (queue as? HandlerSerialQueue)?.handler ?: Handler(Looper.getMainLooper())
        synchronized(lock) {
            this.events = events
            epoch++
            freeInputs.clear()
            submitted.clear()
            busy = false
            firstOutput = false
            inputsBeforeFirstOutput = 0
        }
        val candidates = candidates(mime)
        if (candidates.isEmpty()) throw MediaException.Unsupported()
        for (candidate in candidates) {
            for (keys in KEY_SETS) {
                val mtk = MtkProfile.isMediaTek(candidate.name)
                val format = format(mime, parameters, color, keys, if (mtk) mtkProfile else MtkProfile.NONE)
                val created = try {
                    MediaCodec.createByCodecName(candidate.name)
                } catch (e: Exception) {
                    Log.w(tag, "decoder ${candidate.name} unavailable: $e")
                    break
                }
                try {
                    created.setCallback(Callback(created), handler)
                    created.setOnFrameRenderedListener({ c, presentationTimeUs, nanoTime ->
                        if (c === this.codec) {
                            this.events?.presented(presentationTimeUs.toULong(), nanoTime.toULong(), false)
                        }
                    }, handler)
                    created.configure(format, surface, null, 0)
                    created.start()
                } catch (e: Exception) {
                    Log.w(tag, "configure ${candidate.name} with ${keys.name} keys failed: $e")
                    release(created)
                    continue
                }
                synchronized(lock) { this.codec = created }
                decoderName = candidate.name
                val enabled = enableVendorLowLatency(created, keys) +
                    if (mtk) enableMtkProfile(created, keys) else emptyList()
                profileName = (if (mtk) "mtk ${mtkProfile.name.lowercase()}, " else "") + "keys ${keys.name}"
                val accepted = acceptedKeys(created)
                val confirmed = lowLatencyConfirmed(created, keys)
                // A guess until the first output; ClientStream learns the real depth.
                val depth = if (confirmed || candidate.lowLatency) 1 else 2
                Log.i(
                    tag,
                    "decoder ${candidate.name}: hardware ${candidate.hardware}, FEATURE_LowLatency " +
                        "${candidate.lowLatency}, keys ${keys.name}, low latency confirmed $confirmed, " +
                        "${parameters.width}x${parameters.height}, $color",
                )
                Log.i(tag, "decoder ${candidate.name}: low-latency keys read back: ${accepted.ifEmpty { listOf("none") }}")
                return DecoderInfo(
                    lowLatencyConfirmed = confirmed,
                    hardware = candidate.hardware,
                    maxQueued = depth.toUByte(),
                    notes = listOf(
                        "decoder ${candidate.name}",
                        "keys ${keys.name}",
                        "feature low latency ${candidate.lowLatency}",
                        "vendor parameters set ${enabled.ifEmpty { listOf("none") }}",
                        "keys read back ${accepted.ifEmpty { listOf("none") }}",
                    ),
                )
            }
        }
        throw MediaException.EncoderFailed(true)
    }

    override fun decode(sample: ByteArray, meta: SampleMeta): DecodeResult {
        val codec = synchronized(lock) { codec } ?: return DecodeResult.Failed(MediaException.DeviceLost())
        // Try the free inputs of the current epoch in turn. An index the codec no longer
        // owns (announced before a flush) is dropped, not a failure; with none left the
        // sample is Busy until the next onInputBufferAvailable (input_ready).
        while (true) {
            val index = synchronized(lock) {
                var next: Int? = null
                while (next == null) {
                    val entry = freeInputs.pollFirst() ?: break
                    if ((entry ushr 32).toInt() == epoch) next = entry.toInt()
                }
                if (next == null) busy = true
                next
            } ?: return DecodeResult.Busy
            try {
                val buffer = codec.getInputBuffer(index) ?: continue
                if (sample.size > buffer.capacity()) {
                    synchronized(lock) { freeInputs.addFirst(entry(index)) }
                    return DecodeResult.Failed(MediaException.OutOfResources())
                }
                buffer.clear()
                buffer.put(sample)
                // Length prefixes (Avcc4) to start codes, in place: both are 4 bytes.
                var at = 0
                while (at + 4 <= sample.size) {
                    val length = ((sample[at].toInt() and 0xff) shl 24) or
                        ((sample[at + 1].toInt() and 0xff) shl 16) or
                        ((sample[at + 2].toInt() and 0xff) shl 8) or
                        (sample[at + 3].toInt() and 0xff)
                    buffer.put(at, 0); buffer.put(at + 1, 0); buffer.put(at + 2, 0); buffer.put(at + 3, 1)
                    if (length <= 0) break
                    at += 4 + length
                }
                val flags = if (meta.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                codec.queueInputBuffer(index, 0, sample.size, meta.seq.toLong(), flags)
                synchronized(lock) {
                    submitted[meta.seq.toLong()] = System.nanoTime()
                    if (!firstOutput) inputsBeforeFirstOutput++
                }
                return DecodeResult.Accepted
            } catch (e: MediaCodec.CodecException) {
                if (e.isTransient) {
                    Log.w(tag, "input $index: transient $e; dropping the index")
                    continue
                }
                Log.w(tag, "queueInputBuffer: $e")
                return DecodeResult.Failed(MediaException.EncoderFailed(e.isRecoverable))
            } catch (e: IllegalStateException) {
                // "index N is not owned by client": a stale index. Drop it.
                Log.w(tag, "input $index: $e; dropping the index")
                continue
            }
        }
    }

    override fun flush(): InvalidateOutcome {
        val codec = synchronized(lock) {
            epoch++
            freeInputs.clear()
            submitted.clear()
            busy = false
            codec
        } ?: return InvalidateOutcome.COMPLETED
        val done = helper.submit {
            try {
                codec.flush()
                // Asynchronous mode stops at flush; input callbacks resume after start.
                codec.start()
            } catch (e: Exception) {
                Log.w(tag, "flush: $e")
            }
        }
        return try {
            done.get(FLUSH_BOUND_MS, TimeUnit.MILLISECONDS)
            InvalidateOutcome.COMPLETED
        } catch (e: java.util.concurrent.TimeoutException) {
            Log.w(tag, "flush overran ${FLUSH_BOUND_MS} ms")
            InvalidateOutcome.TIMED_OUT
        } catch (e: Exception) {
            InvalidateOutcome.COMPLETED
        }
    }

    override fun setMode(mode: PresentMode) {
        synchronized(lock) { this.mode = mode }
    }

    override fun nowNs(): ULong = System.nanoTime().toULong()

    /** Decode latency C2 → C3 percentiles in milliseconds (p50, p95) over the last 240. */
    fun decodeLatencyMs(): Pair<Double, Double>? {
        val samples = synchronized(lock) {
            decodeNs.copyOf(minOf(decodeCount, decodeNs.size))
        }
        if (samples.isEmpty()) return null
        samples.sort()
        fun at(q: Double) = samples[((samples.size - 1) * q).toInt()] / 1e6
        return at(0.5) to at(0.95)
    }

    /** Inputs the current decoder took before its first output: its real pipeline depth. */
    fun measuredDepth(): Int = synchronized(lock) { inputsBeforeFirstOutput }

    /** Release the decoder (off the caller's thread: a release can block). */
    fun close() {
        releaseCodec()
        helper.shutdown()
    }

    private fun releaseCodec() {
        val old = synchronized(lock) {
            val old = codec
            codec = null
            events = null
            old
        }
        old?.let { release(it) }
    }

    private fun release(codec: MediaCodec) {
        helper.execute {
            try {
                codec.stop()
            } catch (_: Exception) {
            }
            codec.release()
        }
    }

    /** Whether [candidate] is the session's codec (a replaced codec's callbacks are dropped). */
    private fun isCurrent(candidate: MediaCodec): Boolean = synchronized(lock) { candidate === codec }

    private inner class Callback(private val owner: MediaCodec) : MediaCodec.Callback() {
        override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
            val wake = synchronized(lock) {
                if (!isCurrent(owner)) return
                freeInputs.addLast(entry(index))
                val wasBusy = busy
                busy = false
                wasBusy
            }
            if (wake) events?.inputReady()
        }

        override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            val now = System.nanoTime()
            val (current, mode, events) = synchronized(lock) {
                Triple(isCurrent(owner), mode, events)
            }
            if (!current) return
            val frame = info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0
            try {
                when {
                    !frame -> codec.releaseOutputBuffer(index, false)
                    mode == PresentMode.SMOOTH -> codec.releaseOutputBuffer(index, now)
                    else -> codec.releaseOutputBuffer(index, true)
                }
            } catch (e: IllegalStateException) {
                return
            }
            if (!frame) return
            val seq = info.presentationTimeUs
            synchronized(lock) {
                if (!firstOutput) {
                    Log.i(
                        tag,
                        "first output after $inputsBeforeFirstOutput input(s): depth $inputsBeforeFirstOutput " +
                            "($decoderName, $profileName)",
                    )
                }
                firstOutput = true
                submitted.remove(seq)?.let {
                    decodeNs[decodeCount % decodeNs.size] = now - it
                    decodeCount++
                }
            }
            events?.decoded(seq.toULong(), now.toULong())
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            if (!isCurrent(owner)) return
            Log.e(tag, "decoder error: $e (recoverable ${e.isRecoverable}, transient ${e.isTransient})")
            events?.failed(MediaException.EncoderFailed(e.isRecoverable), e.isRecoverable || e.isTransient)
        }

        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
            Log.i(tag, "output format: $format")
        }
    }

    private class Candidate(val name: String, val hardware: Boolean, val lowLatency: Boolean)

    private fun candidates(mime: String): List<Candidate> =
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
            .filter { !it.isEncoder && it.supportedTypes.any { t -> t.equals(mime, ignoreCase = true) } }
            .map { info ->
                val hardware = if (Build.VERSION.SDK_INT >= 29) {
                    info.isHardwareAccelerated
                } else {
                    !info.name.startsWith("OMX.google.") && !info.name.startsWith("c2.android.")
                }
                val lowLatency = Build.VERSION.SDK_INT >= 30 &&
                    info.getCapabilitiesForType(mime)
                        .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
                Candidate(info.name, hardware, lowLatency)
            }
            .sortedWith(compareByDescending<Candidate> { it.hardware }.thenByDescending { it.lowLatency })

    private class KeySet(val name: String, val lowLatency: Boolean, val vendor: Boolean)

    private fun format(
        mime: String,
        parameters: ParameterSets,
        color: ColorTags,
        keys: KeySet,
        mtk: MtkProfile,
    ): MediaFormat =
        MediaFormat.createVideoFormat(mime, parameters.width, parameters.height).apply {
            parameters.csd.forEachIndexed { i, csd -> setByteBuffer("csd-$i", java.nio.ByteBuffer.wrap(csd)) }
            if (keys.lowLatency) {
                if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
            }
            if (keys.vendor) {
                for ((key, value) in VENDOR_LOW_LATENCY_KEYS) setInteger(key, value)
                for (key in mtk.formatKeys()) setInteger(key, 1)
            }
            colorKeys(color)
        }

    /** BT.709 (or what the generation states) as MediaFormat colour keys (3.3). */
    private fun MediaFormat.colorKeys(color: ColorTags) {
        when (color.matrix) {
            YCbCrMatrix.BT709 -> setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
            YCbCrMatrix.BT601 -> setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT601_NTSC)
            YCbCrMatrix.BT2020 -> setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT2020)
            else -> if (color.primaries == ColorPrimaries.BT709) {
                setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
            }
        }
        when (color.transfer) {
            TransferFunction.BT709, TransferFunction.SRGB ->
                setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
            TransferFunction.LINEAR -> setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_LINEAR)
            TransferFunction.PQ -> setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_ST2084)
            TransferFunction.HLG -> setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_HLG)
            TransferFunction.UNSPECIFIED -> {}
        }
        setInteger(
            MediaFormat.KEY_COLOR_RANGE,
            if (color.fullRange) MediaFormat.COLOR_RANGE_FULL else MediaFormat.COLOR_RANGE_LIMITED,
        )
    }

    /**
     * Android 12+: the codec lists the vendor parameters it really has. Logs them all, and sets
     * every integer one whose name says low latency to 1 (how a MediaTek C2 decoder, whose
     * key names differ between releases, is switched over). Returns the names set.
     */
    private fun enableVendorLowLatency(codec: MediaCodec, keys: KeySet): List<String> {
        if (Build.VERSION.SDK_INT < 31) return emptyList()
        val names = try {
            codec.supportedVendorParameters
        } catch (e: Exception) {
            Log.w(tag, "supportedVendorParameters: $e")
            return emptyList()
        }
        Log.i(tag, "vendor parameters of $decoderName: ${names.sorted()}")
        if (!keys.vendor) return emptyList()
        val wanted = names.filter { LOW_LATENCY_NAME.containsMatchIn(it) }
        val set = wanted.filter { name ->
            try {
                val descriptor = codec.getParameterDescriptor(name)
                if (descriptor?.type != MediaFormat.TYPE_INTEGER) return@filter false
                codec.setParameters(android.os.Bundle().apply { putInt(name, 1) })
                true
            } catch (e: Exception) {
                Log.w(tag, "setting $name: $e")
                false
            }
        }
        if (set.isNotEmpty()) Log.i(tag, "set vendor low-latency parameters: $set")
        return set
    }

    /**
     * The [mtkProfile]'s parameters among those the codec lists (Android 12+), each set to 1 on
     * its own; logs which were accepted. Returns them.
     */
    private fun enableMtkProfile(codec: MediaCodec, keys: KeySet): List<String> {
        if (Build.VERSION.SDK_INT < 31 || !keys.vendor || mtkProfile == MtkProfile.NONE) return emptyList()
        val supported = try {
            codec.supportedVendorParameters
        } catch (e: Exception) {
            return emptyList()
        }
        val accepted = ArrayList<String>()
        for (name in mtkProfile.select(supported)) {
            try {
                if (codec.getParameterDescriptor(name)?.type != MediaFormat.TYPE_INTEGER) {
                    Log.i(tag, "mtk ${mtkProfile.name.lowercase()}: $name is not an integer, skipped")
                    continue
                }
                codec.setParameters(android.os.Bundle().apply { putInt(name, 1) })
                accepted += name
            } catch (e: Exception) {
                Log.w(tag, "mtk ${mtkProfile.name.lowercase()}: $name refused: $e")
            }
        }
        Log.i(tag, "mtk ${mtkProfile.name.lowercase()}: accepted ${accepted.ifEmpty { listOf("none") }}")
        return accepted
    }

    /** The low-latency keys the codec's input or output format reads back as 1. */
    private fun acceptedKeys(codec: MediaCodec): List<String> = try {
        val formats = listOf(codec.inputFormat, codec.outputFormat)
        val keys = buildSet {
            if (Build.VERSION.SDK_INT >= 30) add(MediaFormat.KEY_LOW_LATENCY)
            addAll(VENDOR_LOW_LATENCY_KEYS.keys)
            if (Build.VERSION.SDK_INT >= 29) {
                formats.forEach { f -> addAll(f.keys.filter { LOW_LATENCY_NAME.containsMatchIn(it) }) }
            }
        }
        keys.filter { key ->
            formats.any { f ->
                f.containsKey(key) && try {
                    f.getInteger(key) == 1
                } catch (e: Exception) {
                    false
                }
            }
        }
    } catch (e: Exception) {
        emptyList()
    }

    private fun lowLatencyConfirmed(codec: MediaCodec, keys: KeySet): Boolean =
        (keys.lowLatency || keys.vendor) && acceptedKeys(codec).isNotEmpty()

    private companion object {
        const val FLUSH_BOUND_MS = 450L

        /**
         * Vendor low-latency switches set in the format: Qualcomm, Samsung Exynos, HiSilicon,
         * Amlogic and others. MediaTek decoders have no such key; their switches are the
         * [MtkProfile]. On Android 12+ [enableVendorLowLatency] also sets whatever low-latency
         * parameter the codec itself lists, and the log names the keys that were read back.
         */
        val VENDOR_LOW_LATENCY_KEYS = mapOf(
            "vendor.qti-ext-dec-low-latency.enable" to 1,
            "vendor.rtc-ext-dec-low-latency.enable" to 1,
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req" to 1,
            "vendor.low-latency.enable" to 1,
        )

        /** A vendor parameter name that switches low latency. */
        val LOW_LATENCY_NAME = Regex("low[-_.]?latency", RegexOption.IGNORE_CASE)

        /** Configure attempts per decoder, most keys first. */
        val KEY_SETS = listOf(
            KeySet("low-latency+vendor", lowLatency = true, vendor = true),
            KeySet("low-latency", lowLatency = true, vendor = false),
            KeySet("none", lowLatency = false, vendor = false),
        )
    }
}

/** A configuration record's parameter sets as Annex-B csd buffers, and the picture size. */
internal class ParameterSets(val csd: List<ByteArray>, val width: Int, val height: Int) {
    companion object {
        private val START = byteArrayOf(0, 0, 0, 1)
        private const val FALLBACK_WIDTH = 1920
        private const val FALLBACK_HEIGHT = 1080

        /** An AVCDecoderConfigurationRecord: csd-0 the SPS, csd-1 the PPS. */
        fun fromAvcC(avcc: ByteArray): ParameterSets? {
            if (avcc.size < 7 || avcc[0].toInt() != 1) return null
            var at = 5
            val sps = ArrayList<ByteArray>()
            val pps = ArrayList<ByteArray>()
            repeat(avcc[at++].toInt() and 0x1f) { sps += nal(avcc, at)?.also { at += 2 + it.size } ?: return null }
            if (at >= avcc.size) return null
            repeat(avcc[at++].toInt() and 0xff) { pps += nal(avcc, at)?.also { at += 2 + it.size } ?: return null }
            if (sps.isEmpty() || pps.isEmpty()) return null
            val (w, h) = H264Sps.size(sps[0]) ?: (FALLBACK_WIDTH to FALLBACK_HEIGHT)
            return ParameterSets(
                listOf(sps.fold(ByteArray(0)) { a, n -> a + START + n }, pps.fold(ByteArray(0)) { a, n -> a + START + n }),
                w,
                h,
            )
        }

        /** An HEVCDecoderConfigurationRecord: csd-0 the VPS, SPS and PPS together. */
        fun fromHvcC(hvcc: ByteArray): ParameterSets? {
            if (hvcc.size < 23) return null
            var at = 22
            val arrays = hvcc[at++].toInt() and 0xff
            var csd = ByteArray(0)
            repeat(arrays) {
                if (at + 3 > hvcc.size) return null
                at += 1
                val count = ((hvcc[at].toInt() and 0xff) shl 8) or (hvcc[at + 1].toInt() and 0xff)
                at += 2
                repeat(count) {
                    val unit = nal(hvcc, at) ?: return null
                    at += 2 + unit.size
                    csd += START + unit
                }
            }
            if (csd.isEmpty()) return null
            return ParameterSets(listOf(csd), FALLBACK_WIDTH, FALLBACK_HEIGHT)
        }

        private fun nal(record: ByteArray, at: Int): ByteArray? {
            if (at + 2 > record.size) return null
            val length = ((record[at].toInt() and 0xff) shl 8) or (record[at + 1].toInt() and 0xff)
            if (at + 2 + length > record.size) return null
            return record.copyOfRange(at + 2, at + 2 + length)
        }
    }
}

/** Just enough of an H.264 SPS for the picture size (7.3.2.1.1), cropping applied. */
internal object H264Sps {
    fun size(nal: ByteArray): Pair<Int, Int>? = try {
        val r = Bits(unescape(nal.copyOfRange(1, nal.size)))
        val profile = r.bits(8)
        r.bits(16)
        r.ue()
        var chroma = 1
        if (profile in setOf(100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135)) {
            chroma = r.ue()
            if (chroma == 3) r.bits(1)
            r.ue(); r.ue(); r.bits(1)
            if (r.bits(1) == 1) {
                repeat(if (chroma == 3) 12 else 8) { i ->
                    if (r.bits(1) == 1) {
                        var last = 8
                        var next = 8
                        repeat(if (i < 6) 16 else 64) {
                            if (next != 0) next = (last + r.se() + 256) % 256
                            if (next != 0) last = next
                        }
                    }
                }
            }
        }
        r.ue()
        when (r.ue()) {
            0 -> r.ue()
            1 -> {
                r.bits(1); r.se(); r.se()
                repeat(r.ue()) { r.se() }
            }
        }
        r.ue(); r.bits(1)
        val widthMbs = r.ue() + 1
        val heightUnits = r.ue() + 1
        val frameMbsOnly = r.bits(1)
        if (frameMbsOnly == 0) r.bits(1)
        r.bits(1)
        var width = widthMbs * 16
        var height = heightUnits * 16 * (2 - frameMbsOnly)
        if (r.bits(1) == 1) {
            val cropX = if (chroma == 0 || chroma == 3) 1 else 2
            val cropY = (if (chroma == 1) 2 else 1) * (2 - frameMbsOnly)
            val left = r.ue(); val right = r.ue(); val top = r.ue(); val bottom = r.ue()
            width -= (left + right) * cropX
            height -= (top + bottom) * cropY
        }
        if (width > 0 && height > 0) width to height else null
    } catch (e: IndexOutOfBoundsException) {
        null
    }

    private fun unescape(data: ByteArray): ByteArray {
        val out = java.io.ByteArrayOutputStream(data.size)
        var zeros = 0
        for (b in data) {
            if (zeros >= 2 && b.toInt() == 3) {
                zeros = 0
                continue
            }
            zeros = if (b.toInt() == 0) zeros + 1 else 0
            out.write(b.toInt())
        }
        return out.toByteArray()
    }

    private class Bits(val data: ByteArray) {
        var at = 0
        fun bits(n: Int): Int {
            var v = 0
            repeat(n) {
                val bit = (data[at / 8].toInt() shr (7 - at % 8)) and 1
                at++
                v = (v shl 1) or bit
            }
            return v
        }
        fun ue(): Int {
            var zeros = 0
            while (bits(1) == 0) {
                zeros++
                if (zeros > 31) throw IndexOutOfBoundsException()
            }
            return (1 shl zeros) - 1 + bits(zeros)
        }
        fun se(): Int {
            val k = ue()
            return if (k % 2 == 1) (k + 1) / 2 else -(k / 2)
        }
    }
}
