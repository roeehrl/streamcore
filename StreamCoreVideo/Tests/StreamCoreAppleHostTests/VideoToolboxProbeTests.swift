#if os(macOS)
import CoreMedia
import CoreVideo
import Foundation
import StreamCoreVideo
import Testing
import VideoToolbox
@testable import StreamCoreAppleHost

/// LTR and two temporal layers on VideoToolbox H.264 (design 6.4, 6.5): the probe with real
/// sessions, and how an output's attachments become the wire's layer and LTR marks.
@Suite(.serialized) struct VideoToolboxProbeTests {
    @Test func theProbeConfirmsLtrAndTemporalLayersOnHardwareH264() {
        let caps = VideoToolboxProbe.run(.h264)
        #expect(caps.codec == .h264)
        #expect(caps.lowLatencyRateControl == .confirmed)
        #if arch(arm64)
        // Apple silicon's encoder took both settings and its output showed them.
        #expect(caps.ltr == .confirmed)
        #expect(caps.temporalLayers == .confirmed)
        #endif
        #expect(caps.intraRefresh == .unsupported, "VideoToolbox has no intra refresh")
    }

    @Test func hevcAndUnavailableSessionsAreNotConfirmed() {
        let hevc = VideoToolboxProbe.run(.hevc)
        #expect(hevc.ltr == .unsupported && hevc.temporalLayers == .unsupported)
        let refused = VideoToolboxProbe.run(.h264) { _, _, _, _ in (kVTCouldNotFindVideoEncoderErr, nil) }
        #expect(refused.ltr == .unsupported && refused.temporalLayers == .unsupported)
    }

    /// The software H.264 encoder (no hardware session): whatever it accepts, the probe
    /// confirms a feature only when its output carried it.
    @Test func aSoftwareSessionIsConfirmedOnlyByItsOutput() {
        let caps = VideoToolboxProbe.run(.h264) { width, height, codec, _ in
            let software = [kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: false] as CFDictionary
            return VideoToolboxSession.create(width: width, height: height, codec: codec, specification: software)
        }
        #expect(!caps.hardware)
        print("VideoToolbox software H.264 probe: ltr=\(caps.ltr) temporalLayers=\(caps.temporalLayers)")
        #expect(caps.ltr != .confirmed && caps.temporalLayers != .confirmed)
    }

    @Test func theProbeReportsToTheProductsLogOncePerCodec() {
        let log = RecordingLog()
        _ = VideoToolboxProbe.caps(.hevc, log: log)
        _ = VideoToolboxProbe.caps(.hevc, log: log)
        #expect(log.lines.filter { $0.contains("HEVC probe") }.count <= 1, "cached after the first probe")
    }

    @Test func attachmentsMapToTheWireTokensAndLayers() {
        typealias E = VideoToolboxStreamEncoder
        #expect(E.wireToken(0) == 1 && E.vtToken(1) == 0 && E.vtToken(0) == nil && E.wireToken(-1) == nil)
        let mark: [CFString: Any] = [kVTSampleAttachmentKey_RequireLTRAcknowledgementToken: NSNumber(value: 4),
                                     kCMSampleAttachmentKey_IsDependedOnByOthers: false]
        let layer1 = E.layerInfo(mark, keyframe: false, ltr: true, temporalLayers: true, refreshFrom: nil)
        #expect(layer1 == E.LayerInfo(discardable: true, temporalID: 1, ltr: LtrInfo(token: 5, flags: 0x01)))
        // Without temporal layers nothing is discardable.
        #expect(E.layerInfo(mark, keyframe: false, ltr: true, temporalLayers: false, refreshFrom: nil).discardable == false)
        // A refresh is predicted from the acknowledged token and is a base-layer frame.
        let refresh = E.layerInfo([kCMSampleAttachmentKey_IsDependedOnByOthers: true, "ReferenceWasRefreshed" as CFString: true],
                                  keyframe: false, ltr: true, temporalLayers: true, refreshFrom: 3)
        #expect(refresh == E.LayerInfo(discardable: false, temporalID: 0, ltr: LtrInfo(token: 3, flags: 0x02)))
        // A refresh that came out as a layer-1 frame repairs nothing: the next layer-0 frame
        // still predicts from the lost one, although VideoToolbox says it refreshed.
        let layer1Refresh = E.layerInfo(mark.merging(["ReferenceWasRefreshed" as CFString: true]) { $1 },
                                        keyframe: false, ltr: true, temporalLayers: true, refreshFrom: 3)
        #expect(layer1Refresh == E.LayerInfo(discardable: true, temporalID: 1, ltr: LtrInfo(token: 5, flags: 0x01)))
        // Not refreshed (or a keyframe instead): no refresh flag, so StreamCore escalates.
        let refused = E.layerInfo(["ReferenceWasRefreshed" as CFString: false], keyframe: false, ltr: true,
                                  temporalLayers: false, refreshFrom: 3)
        #expect(refused.ltr == nil)
        #expect(E.layerInfo([:], keyframe: true, ltr: true, temporalLayers: true, refreshFrom: 3).ltr == nil)
    }
}

final class RecordingLog: MediaLog, @unchecked Sendable {
    private let lock = NSLock()
    private var kept: [String] = []
    var lines: [String] { lock.withLock { kept } }
    func debug(_ message: @autoclosure () -> String) { let line = message(); lock.withLock { kept.append(line) } }
    func info(_ message: @autoclosure () -> String) { let line = message(); lock.withLock { kept.append(line) } }
    func error(_ message: @autoclosure () -> String) { let line = message(); lock.withLock { kept.append(line) } }
}
#endif
