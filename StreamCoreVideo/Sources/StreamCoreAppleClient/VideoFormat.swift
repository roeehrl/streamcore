import CoreMedia
import Foundation
import StreamCoreVideo

/// Compressed video as CoreMedia wants it: the format description for a stream's codec
/// configuration, and each length-prefixed sample as a `CMSampleBuffer`.
public enum VideoFormat {
    public enum FormatError: Error, Equatable {
        case invalidDimensions
        case invalidCodecConfig
        case unsupportedCodec
        case blockBufferFailed(OSStatus)
        case sampleBufferFailed(OSStatus)
        case formatDescriptionFailed(OSStatus)
    }

    /// The format description for `config`, the raw `avcC` (H.264) or `hvcC` (HEVC) atom
    /// body. Its colour extensions come from the parameter sets themselves: a description
    /// made from the atom alone omits them, and decoders then pick a matrix by size.
    /// `size`, when given, must match the parameter sets' dimensions.
    public static func description(codec: Codec, config: Data,
                                   size: (width: Int, height: Int)? = nil) throws -> CMVideoFormatDescription {
        if let size {
            guard (2...8192).contains(size.width), (2...8192).contains(size.height) else { throw FormatError.invalidDimensions }
        }
        let atomKey: String
        let subType: CMVideoCodecType
        switch codec {
        case .hevc: atomKey = "hvcC"; subType = kCMVideoCodecType_HEVC
        case .h264: atomKey = "avcC"; subType = kCMVideoCodecType_H264
        default: throw FormatError.unsupportedCodec
        }
        let extensions: [CFString: Any] = [
            kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: [atomKey: config] as [String: Any],
        ]
        var description: CMVideoFormatDescription?
        // The atom carries the real dimensions; these are placeholders until it is parsed.
        let status = CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: subType, width: Int32(size?.width ?? 2),
            height: Int32(size?.height ?? 2), extensions: extensions as CFDictionary,
            formatDescriptionOut: &description)
        guard status == noErr, let description else { throw FormatError.formatDescriptionFailed(status) }
        let getParameterSet = codec == .h264
            ? CMVideoFormatDescriptionGetH264ParameterSetAtIndex
            : CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
        var count = 0, headerLength: Int32 = 0
        guard getParameterSet(description, 0, nil, nil, &count, &headerLength) == noErr,
              (1...1024).contains(count) else { throw FormatError.invalidCodecConfig }
        var pointers: [UnsafePointer<UInt8>] = [], sizes: [Int] = []
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?, length = 0
            guard getParameterSet(description, index, &pointer, &length, nil, nil) == noErr,
                  let pointer, length > 0 else { throw FormatError.invalidCodecConfig }
            pointers.append(pointer); sizes.append(length)
        }
        var parsed: CMVideoFormatDescription?
        let parsedStatus = withExtendedLifetime(description) {
            pointers.withUnsafeBufferPointer { pointers in
                sizes.withUnsafeBufferPointer { sizes in
                    if codec == .h264 {
                        return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                            allocator: kCFAllocatorDefault, parameterSetCount: count,
                            parameterSetPointers: pointers.baseAddress!, parameterSetSizes: sizes.baseAddress!,
                            nalUnitHeaderLength: headerLength, formatDescriptionOut: &parsed)
                    }
                    return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault, parameterSetCount: count,
                        parameterSetPointers: pointers.baseAddress!, parameterSetSizes: sizes.baseAddress!,
                        nalUnitHeaderLength: headerLength, extensions: nil, formatDescriptionOut: &parsed)
                }
            }
        }
        guard parsedStatus == noErr, let parsed else { throw FormatError.formatDescriptionFailed(parsedStatus) }
        let dimensions = CMVideoFormatDescriptionGetDimensions(parsed)
        guard (2...8192).contains(Int(dimensions.width)), (2...8192).contains(Int(dimensions.height)) else {
            throw FormatError.invalidDimensions
        }
        if let size {
            guard Int(dimensions.width) == size.width, Int(dimensions.height) == size.height else {
                throw FormatError.invalidCodecConfig
            }
        }
        return parsed
    }

    /// One length-prefixed (AVCC/HVCC) sample as a `CMSampleBuffer`, marked "display
    /// immediately". `ptsMicros` is clamped: a crafted value with the high bit set would
    /// otherwise trap in `CMTimeValue`.
    public static func sampleBuffer(_ data: Data, format: CMVideoFormatDescription, ptsMicros: UInt64,
                                    keyframe: Bool) throws -> CMSampleBuffer {
        var blockBuffer: CMBlockBuffer?
        let length = data.count
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: length, blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil, offsetToData: 0, dataLength: length, flags: 0, blockBufferOut: &blockBuffer)
        guard status == kCMBlockBufferNoErr, let blockBuffer else { throw FormatError.blockBufferFailed(status) }
        status = data.withUnsafeBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return OSStatus(kCMBlockBufferBadPointerParameterErr) }
            return CMBlockBufferReplaceDataBytes(with: base, blockBuffer: blockBuffer,
                                                 offsetIntoDestination: 0, dataLength: length)
        }
        guard status == kCMBlockBufferNoErr else { throw FormatError.blockBufferFailed(status) }
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: CMTimeValue(min(ptsMicros, UInt64(Int64.max))), timescale: 1_000_000),
            decodeTimeStamp: .invalid)
        var sampleSize = length
        var sampleBuffer: CMSampleBuffer?
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: blockBuffer, formatDescription: format, sampleCount: 1,
            sampleTimingEntryCount: 1, sampleTimingArray: &timing, sampleSizeEntryCount: 1,
            sampleSizeArray: &sampleSize, sampleBufferOut: &sampleBuffer)
        guard status == noErr, let sampleBuffer else { throw FormatError.sampleBufferFailed(status) }
        sampleBuffer.markForImmediateDisplay(dependsOnOthers: !keyframe)
        return sampleBuffer
    }
}

extension CMSampleBuffer {
    /// "Display immediately" (R2): the layer shows the sample as soon as it has it,
    /// ignoring its timestamp. A dependent sample is also marked as not a sync sample.
    public func markForImmediateDisplay(dependsOnOthers: Bool) {
        guard let attachmentsArray = CMSampleBufferGetSampleAttachmentsArray(self, createIfNecessary: true),
              CFArrayGetCount(attachmentsArray) > 0 else { return }
        let attachments = unsafeBitCast(CFArrayGetValueAtIndex(attachmentsArray, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(attachments,
                             Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                             Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        guard dependsOnOthers else { return }
        CFDictionarySetValue(attachments,
                             Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque(),
                             Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        CFDictionarySetValue(attachments,
                             Unmanaged.passUnretained(kCMSampleAttachmentKey_DependsOnOthers).toOpaque(),
                             Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
    }
}
