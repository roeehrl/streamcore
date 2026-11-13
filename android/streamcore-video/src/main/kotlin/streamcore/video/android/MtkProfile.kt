package streamcore.video.android

/**
 * Which MediaTek decoder switches [MediaCodecDecoder] sets on a MediaTek (`c2.mtk.*`,
 * `OMX.MTK.*`) decoder, for A/B tests of their effect on the decoder's depth and decode
 * time. Each parameter is set on its own and logged as accepted or not; a parameter the codec
 * does not list is skipped. `ALL` is the default.
 */
enum class MtkProfile(val vpp: Boolean, val vilte: Boolean, val cpuBoost: Boolean) {
    /** Nothing MediaTek-specific. */
    NONE(vpp = false, vilte = false, cpuBoost = false),

    /** Post-processing off: `vendor.mtk.ext.vdec.vpp.*disabled*` = 1 (motion interpolation,
     *  quality tuner, resolution scaler, and VPP as a whole). */
    VPP(vpp = true, vilte = false, cpuBoost = false),

    /** The video-calling real-time mode: `vendor.mtk.ext.vdec.vilte.feature-on` = 1. */
    VILTE(vpp = false, vilte = true, cpuBoost = false),

    /** VPP off, ViLTE mode and `vendor.mtk.vdec.cpu.boost.mode.value` = 1. */
    ALL(vpp = true, vilte = true, cpuBoost = true);

    /** The parameters of this profile among those the codec lists (Android 12+). */
    fun select(supported: Collection<String>): List<String> = supported.filter { name ->
        (vpp && name.contains(".vpp.") && name.contains("disabled")) ||
            (vilte && name.contains("vilte.feature-on")) ||
            (cpuBoost && name.contains("cpu.boost.mode"))
    }.sorted()

    /** The profile's parameters by full name, also put in the configure format. */
    fun formatKeys(): List<String> = buildList {
        if (vpp) add("vendor.mtk.ext.vdec.vpp.disabled.value")
        if (vilte) add("vendor.mtk.ext.vdec.vilte.feature-on")
        if (cpuBoost) add("vendor.mtk.vdec.cpu.boost.mode.value")
    }

    companion object {
        /** `none`, `vpp`, `vilte` or `all` (any case); anything else is `ALL`. */
        fun parse(name: String?): MtkProfile =
            entries.firstOrNull { it.name.equals(name?.trim(), ignoreCase = true) } ?: ALL

        fun isMediaTek(decoder: String): Boolean =
            decoder.startsWith("c2.mtk.", ignoreCase = true) || decoder.startsWith("OMX.MTK.", ignoreCase = true)
    }
}
