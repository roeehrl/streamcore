// macOS only: the host side (a Mac captures and encodes).
#if os(macOS)
import Accelerate
import CoreVideo
import Foundation
import Metal
import StreamCoreVideo

/// Which compositor a combined canvas uses (design 3.9): `metal` composes on the GPU,
/// `cpu` composes BGRA rows on the CPU and converts to NV12 with vImage.
public enum CanvasCompositorKind: String, Sendable {
    case metal, cpu
}

// MARK: - Colour

/// The colour arithmetic both compositors and their tests share (BT.709 / Display P3,
/// sRGB or BT.709 transfer, YCbCr with the plan's matrix and range).
public enum CanvasColor {
    /// Linear-light RGB from P3 (D65) to BT.709 primaries, and back.
    public static let p3To709: [Float] = [1.2249, -0.2247, 0.0, -0.0420, 1.0419, 0.0, -0.0197, -0.0786, 1.0979]
    public static let bt709ToP3: [Float] = [0.8225, 0.1774, 0.0, 0.0332, 0.9669, 0.0, 0.0171, 0.0724, 0.9108]
    public static let identity: [Float] = [1, 0, 0, 0, 1, 0, 0, 0, 1]

    /// The gamut matrix (rows) from a member's primaries to the canvas's.
    public static func gamut(from source: ColorPrimaries, to output: ColorPrimaries) -> [Float] {
        switch (source == .displayP3, output == .displayP3) {
        case (true, false): p3To709
        case (false, true): bt709ToP3
        default: identity
        }
    }

    /// 0: sRGB (also unspecified), 1: BT.709, 2: linear.
    public static func transferCode(_ transfer: TransferFunction) -> UInt32 {
        switch transfer {
        case .bt709: 1
        case .linear: 2
        default: 0
        }
    }

    /// (Kr, Kb) of the canvas's YCbCr matrix: BT.601 or BT.709 (also unspecified).
    public static func coefficients(_ matrix: YCbCrMatrix) -> (Float, Float) {
        matrix == .bt601 ? (0.299, 0.114) : (0.2126, 0.0722)
    }

    public static func attach(_ tags: ColorTags, to buffer: CVPixelBuffer) {
        let values = SurfaceColor.properties(tags)
        for (key, value) in [(kCVImageBufferColorPrimariesKey, values.primaries),
                             (kCVImageBufferTransferFunctionKey, values.transfer),
                             (kCVImageBufferYCbCrMatrixKey, values.matrix)] {
            if let value { CVBufferSetAttachment(buffer, key, value, .shouldPropagate) }
        }
    }
}

// MARK: - Metal

/// StreamCore's `Compositor` on Metal (design 3.9): one compute pass draws every member's
/// latest BGRA surface into its tile of an NV12 canvas from a pool of `plan.poolSize`
/// (permits + 2), every gap black. The shader linearises each member with its own
/// transfer, maps its gamut to the canvas's (Display P3 ↔ BT.709), re-encodes the canvas
/// transfer and applies the canvas's YCbCr matrix and range, so VideoToolbox encodes the
/// surface as is. `compose` records the pass and returns at once: the composite's fence is
/// this compositor's `MTLSharedEvent` (handle 0) at the pass's value, and the encoder
/// waits for it before VideoToolbox reads the surface. The pass keeps its member buffers
/// alive until the GPU completes. Every call except `release` runs on the stream executor.
public final class MetalCanvasCompositor: Compositor, @unchecked Sendable {
    public static let maxMembers = 8
    public let surfaces: SurfaceStore
    public let event: MTLSharedEvent
    private let queue: MTLCommandQueue
    private let pipeline: MTLComputePipelineState
    private let textureCache: CVMetalTextureCache
    private let placeholder: MTLTexture
    private let lock = NSLock()
    private var plan: CanvasPlan?
    private var pool: CVPixelBufferPool?
    private var signalled: UInt64 = 0

    public init(surfaces: SurfaceStore) throws {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue(),
              let event = device.makeSharedEvent() else { throw MediaError.Unsupported }
        let library = try device.makeLibrary(source: Self.shader, options: nil)
        guard let function = library.makeFunction(name: "compose_canvas") else { throw MediaError.Internal }
        pipeline = try device.makeComputePipelineState(function: function)
        var cache: CVMetalTextureCache?
        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &cache) == kCVReturnSuccess, let cache else {
            throw MediaError.Unsupported
        }
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: 1, height: 1, mipmapped: false)
        guard let placeholder = device.makeTexture(descriptor: descriptor) else { throw MediaError.OutOfResources }
        self.surfaces = surfaces; self.queue = queue; self.event = event
        textureCache = cache; self.placeholder = placeholder
    }

    public func configure(plan: CanvasPlan) throws {
        guard plan.tiles.count <= Self.maxMembers, plan.poolSize >= 2 else { throw MediaError.InvalidSettings }
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: plan.color.fullRange
                ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey: Int(plan.width), kCVPixelBufferHeightKey: Int(plan.height),
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        var created: CVPixelBufferPool?
        guard CVPixelBufferPoolCreate(nil, [kCVPixelBufferPoolMinimumBufferCountKey: Int(plan.poolSize)] as CFDictionary,
                                      attributes as CFDictionary, &created) == kCVReturnSuccess, let created else {
            throw MediaError.OutOfResources
        }
        lock.withLock { self.plan = plan; pool = created }
    }

    public func compose(members: [MemberSurface]) throws -> ComposeResult {
        guard let (plan, pool) = lock.withLock({ () -> (CanvasPlan, CVPixelBufferPool)? in
            guard let plan, let pool else { return nil }
            return (plan, pool)
        }) else { throw MediaError.Internal }
        var output: CVPixelBuffer?
        let aux = [kCVPixelBufferPoolAllocationThresholdKey: Int(plan.poolSize)] as CFDictionary
        let status = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(nil, pool, aux, &output)
        if status == kCVReturnWouldExceedAllocationThreshold { return .poolExhausted }
        guard status == kCVReturnSuccess, let canvas = output else { throw MediaError.OutOfResources }
        guard let luma = texture(canvas, plane: 0, format: .r8Unorm),
              let chroma = texture(canvas, plane: 1, format: .rg8Unorm) else { throw MediaError.Internal }

        // Member textures and per-tile parameters, in tile order.
        var tiles: [TileParameters] = []
        var textures: [MTLTexture] = []
        var retained: [AnyObject] = [luma.cv, chroma.cv]
        for tile in plan.tiles.prefix(Self.maxMembers) {
            guard let member = members.first(where: { $0.member == tile.member }),
                  let entry = surfaces.entry(member.surface),
                  CVPixelBufferGetPixelFormatType(entry.image) == kCVPixelFormatType_32BGRA,
                  let source = texture(entry.image, plane: nil, format: .bgra8Unorm) else {
                // Rust composes only with every member present; a missing one is a bug.
                throw MediaError.Internal
            }
            let tags = SurfaceColor.tags(of: entry.image)
            let gamut = CanvasColor.gamut(from: tags.primaries, to: plan.color.primaries)
            tiles.append(TileParameters(
                rect: SIMD4(Int32(tile.rect.x), Int32(tile.rect.y), Int32(tile.rect.width), Int32(tile.rect.height)),
                g0: SIMD4(gamut[0], gamut[1], gamut[2], 0), g1: SIMD4(gamut[3], gamut[4], gamut[5], 0),
                g2: SIMD4(gamut[6], gamut[7], gamut[8], 0),
                flags: SIMD4(CanvasColor.transferCode(tags.transfer), Self.quarterTurns(entry.rotation), 0, 0)))
            textures.append(source.texture)
            retained.append(source.cv); retained.append(entry.image)
        }
        let (kr, kb) = CanvasColor.coefficients(plan.color.matrix)
        var parameters = CanvasParameters(
            size: SIMD4(plan.width, plan.height, UInt32(tiles.count), CanvasColor.transferCode(plan.color.transfer)),
            yuv: SIMD4(kr, kb, plan.color.fullRange ? 0 : 1, 0))
        while textures.count < Self.maxMembers { textures.append(placeholder) }
        if tiles.isEmpty { tiles.append(TileParameters.empty) }

        guard let commands = queue.makeCommandBuffer(), let encoder = commands.makeComputeCommandEncoder() else {
            throw MediaError.Internal
        }
        encoder.setComputePipelineState(pipeline)
        encoder.setTexture(luma.texture, index: 0)
        encoder.setTexture(chroma.texture, index: 1)
        encoder.setTextures(textures, range: 2..<(2 + Self.maxMembers))
        encoder.setBytes(&parameters, length: MemoryLayout<CanvasParameters>.stride, index: 0)
        encoder.setBytes(tiles, length: MemoryLayout<TileParameters>.stride * tiles.count, index: 1)
        let blocks = MTLSize(width: (Int(plan.width) + 1) / 2, height: (Int(plan.height) + 1) / 2, depth: 1)
        let group = MTLSize(width: 16, height: 16, depth: 1)
        encoder.dispatchThreads(blocks, threadsPerThreadgroup: group)
        encoder.endEncoding()
        let value = lock.withLock { () -> UInt64 in signalled += 1; return signalled }
        commands.encodeSignalEvent(event, value: value)
        // The members and the canvas stay alive until the GPU is done with them.
        let keep = Retained(retained, cache: textureCache)
        commands.addCompletedHandler { _ in withExtendedLifetime(keep) {} }
        commands.commit()

        CanvasColor.attach(plan.color, to: canvas)
        let id = surfaces.insert(canvas, h0: 0, h1: 0, fence: SurfaceStore.Fence(event: event, value: value))
        return .composed(surface: SurfaceInfo(
            id: id, kind: .ioSurface, format: .nv12, width: plan.width, height: plan.height, color: plan.color,
            captureNs: MediaClock.now(), sourcePresentNs: nil, dirty: nil, rotation: .none,
            fence: .mtlSharedEvent(handle: 0, value: value)))
    }

    public func release(surface: UInt64) { _ = surfaces.remove(surface) }

    /// Clockwise quarter turns that show a member upright.
    public static func quarterTurns(_ rotation: Rotation) -> UInt32 {
        switch rotation {
        case .none: 0
        case .rotate90: 1
        case .rotate180: 2
        case .rotate270: 3
        }
    }

    private struct Texture { let cv: CVMetalTexture; let texture: MTLTexture }

    /// Objects the GPU reads until its completion handler runs. The texture cache must
    /// outlive its textures (a `CVMetalTexture` finalized after its cache crashes in
    /// CoreVideo), and the completion may run after the compositor is gone: the cache is
    /// held here too and released after the textures.
    private final class Retained: @unchecked Sendable {
        private var objects: [AnyObject]
        // periphery:ignore - released after `objects`, see deinit
        private let cache: CVMetalTextureCache
        init(_ objects: [AnyObject], cache: CVMetalTextureCache) { self.objects = objects; self.cache = cache }
        deinit { objects.removeAll() }
    }

    private func texture(_ buffer: CVPixelBuffer, plane: Int?, format: MTLPixelFormat) -> Texture? {
        let width = plane.map { CVPixelBufferGetWidthOfPlane(buffer, $0) } ?? CVPixelBufferGetWidth(buffer)
        let height = plane.map { CVPixelBufferGetHeightOfPlane(buffer, $0) } ?? CVPixelBufferGetHeight(buffer)
        var created: CVMetalTexture?
        guard CVMetalTextureCacheCreateTextureFromImage(nil, textureCache, buffer, nil, format, width, height,
                                                        plane ?? 0, &created) == kCVReturnSuccess,
              let created, let texture = CVMetalTextureGetTexture(created) else { return nil }
        return Texture(cv: created, texture: texture)
    }

    /// The shader's `Tile`: Swift only writes these fields.
    private struct TileParameters {
        // periphery:ignore - read by the shader
        var rect: SIMD4<Int32>
        // periphery:ignore - read by the shader
        var g0: SIMD4<Float>, g1: SIMD4<Float>, g2: SIMD4<Float>
        // periphery:ignore - read by the shader
        var flags: SIMD4<UInt32>
        static let empty = TileParameters(rect: .zero, g0: .zero, g1: .zero, g2: .zero, flags: .zero)
    }

    /// The shader's `Params`: Swift only writes these fields.
    private struct CanvasParameters {
        /// width, height, tile count, canvas transfer.
        // periphery:ignore - read by the shader
        var size: SIMD4<UInt32>
        /// Kr, Kb, limited range (1) or full (0).
        // periphery:ignore - read by the shader
        var yuv: SIMD4<Float>
    }

    static let shader = """
    #include <metal_stdlib>
    using namespace metal;

    struct Tile { int4 rect; float4 g0; float4 g1; float4 g2; uint4 flags; };
    struct Params { uint4 size; float4 yuv; };

    static float eotf(float v, uint transfer) {
        if (transfer == 2) return v;
        if (transfer == 1) return v < 0.081 ? v / 4.5 : pow((v + 0.099) / 1.099, 1.0 / 0.45);
        return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4);
    }
    static float oetf(float v, uint transfer) {
        v = clamp(v, 0.0, 1.0);
        if (transfer == 2) return v;
        if (transfer == 1) return v < 0.018 ? 4.5 * v : 1.099 * pow(v, 0.45) - 0.099;
        return v <= 0.0031308 ? 12.92 * v : 1.055 * pow(v, 1.0 / 2.4) - 0.055;
    }

    static float3 canvas_rgb(uint2 xy, constant Params& p, constant Tile* tiles,
                             array<texture2d<float, access::sample>, 8> members) {
        constexpr sampler s(coord::normalized, address::clamp_to_edge, filter::linear);
        for (uint i = 0; i < p.size.z; i++) {
            int4 r = tiles[i].rect;
            int2 at = int2(xy);
            if (at.x >= r.x && at.y >= r.y && at.x < r.x + r.z && at.y < r.y + r.w) {
                float2 q = (float2(at - r.xy) + 0.5) / float2(r.zw);
                // The upright tile's point in the stored image, turned clockwise by flags.y.
                uint turns = tiles[i].flags.y;
                float2 uv = turns == 1 ? float2(q.y, 1.0 - q.x)
                          : turns == 2 ? float2(1.0 - q.x, 1.0 - q.y)
                          : turns == 3 ? float2(1.0 - q.y, q.x) : q;
                float3 c = members[i].sample(s, uv).rgb;
                uint t = tiles[i].flags.x;
                float3 lin = float3(eotf(c.r, t), eotf(c.g, t), eotf(c.b, t));
                float3 out = float3(dot(tiles[i].g0.xyz, lin), dot(tiles[i].g1.xyz, lin), dot(tiles[i].g2.xyz, lin));
                return float3(oetf(out.r, p.size.w), oetf(out.g, p.size.w), oetf(out.b, p.size.w));
            }
        }
        return float3(0.0); // gaps are black
    }

    kernel void compose_canvas(texture2d<float, access::write> luma [[texture(0)]],
                               texture2d<float, access::write> chroma [[texture(1)]],
                               array<texture2d<float, access::sample>, 8> members [[texture(2)]],
                               constant Params& p [[buffer(0)]],
                               constant Tile* tiles [[buffer(1)]],
                               uint2 block [[thread_position_in_grid]]) {
        uint2 origin = block * 2;
        if (origin.x >= p.size.x || origin.y >= p.size.y) return;
        float kr = p.yuv.x, kb = p.yuv.y, kg = 1.0 - kr - kb;
        bool limited = p.yuv.z > 0.5;
        float3 sum = float3(0.0);
        float n = 0.0;
        for (uint dy = 0; dy < 2; dy++) {
            for (uint dx = 0; dx < 2; dx++) {
                uint2 xy = origin + uint2(dx, dy);
                if (xy.x >= p.size.x || xy.y >= p.size.y) continue;
                float3 rgb = canvas_rgb(xy, p, tiles, members);
                float y = kr * rgb.r + kg * rgb.g + kb * rgb.b;
                luma.write(float4(limited ? (16.0 + 219.0 * y) / 255.0 : y), xy);
                sum += rgb; n += 1.0;
            }
        }
        float3 rgb = sum / max(n, 1.0);
        float y = kr * rgb.r + kg * rgb.g + kb * rgb.b;
        float cb = (rgb.b - y) / (2.0 * (1.0 - kb));
        float cr = (rgb.r - y) / (2.0 * (1.0 - kr));
        float2 c = limited ? (128.0 + 224.0 * float2(cb, cr)) / 255.0 : float2(cb, cr) + 128.0 / 255.0;
        chroma.write(float4(c, 0.0, 0.0), block);
    }
    """
}

// MARK: - CPU

/// StreamCore's `Compositor` on the CPU (the Metal compositor's rollback and the
/// benchmark's baseline): a row copy into a BGRA scratch canvas (members captured as sRGB at their
/// tile size, gaps opaque black), then vImage's BGRA → NV12 with the canvas's matrix and
/// range. No gamut mapping: ScreenCaptureKit delivers sRGB members; no turn: a member surface
/// with a `rotation` is copied as stored. Synchronous; the composite's fence is `none`.
public final class CPUCanvasCompositor: Compositor, @unchecked Sendable {
    public let surfaces: SurfaceStore
    private let lock = NSLock()
    private var plan: CanvasPlan?
    private var pool: CVPixelBufferPool?
    private var scratch: CVPixelBuffer?
    private var conversion = vImage_ARGBToYpCbCr()

    public init(surfaces: SurfaceStore) { self.surfaces = surfaces }

    public func configure(plan: CanvasPlan) throws {
        var created: CVPixelBufferPool?
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: plan.color.fullRange
                ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey: Int(plan.width), kCVPixelBufferHeightKey: Int(plan.height),
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
        ]
        guard CVPixelBufferPoolCreate(nil, [kCVPixelBufferPoolMinimumBufferCountKey: Int(plan.poolSize)] as CFDictionary,
                                      attributes as CFDictionary, &created) == kCVReturnSuccess, let created else {
            throw MediaError.OutOfResources
        }
        var scratch: CVPixelBuffer?
        guard CVPixelBufferCreate(nil, Int(plan.width), Int(plan.height), kCVPixelFormatType_32BGRA, nil, &scratch)
                == kCVReturnSuccess, let scratch else { throw MediaError.OutOfResources }
        var range = plan.color.fullRange
            ? vImage_YpCbCrPixelRange(Yp_bias: 0, CbCr_bias: 128, YpRangeMax: 255, CbCrRangeMax: 255,
                                      YpMax: 255, YpMin: 0, CbCrMax: 255, CbCrMin: 0)
            : vImage_YpCbCrPixelRange(Yp_bias: 16, CbCr_bias: 128, YpRangeMax: 235, CbCrRangeMax: 240,
                                      YpMax: 235, YpMin: 16, CbCrMax: 240, CbCrMin: 16)
        let (kr, kb) = CanvasColor.coefficients(plan.color.matrix)
        let kg = 1 - kr - kb
        var matrix = vImage_ARGBToYpCbCrMatrix(
            R_Yp: kr, G_Yp: kg, B_Yp: kb,
            R_Cb: -kr / (2 * (1 - kb)), G_Cb: -kg / (2 * (1 - kb)), B_Cb_R_Cr: 0.5,
            G_Cr: -kg / (2 * (1 - kr)), B_Cr: -kb / (2 * (1 - kr)))
        var info = vImage_ARGBToYpCbCr()
        guard vImageConvert_ARGBToYpCbCr_GenerateConversion(&matrix, &range, &info, kvImageARGB8888,
                                                            kvImage420Yp8_CbCr8, vImage_Flags(kvImageNoFlags))
                == kvImageNoError else { throw MediaError.Unsupported }
        lock.withLock { self.plan = plan; pool = created; self.scratch = scratch; conversion = info }
    }

    public func compose(members: [MemberSurface]) throws -> ComposeResult {
        guard let (plan, pool, scratch, info) = lock.withLock({ () -> (CanvasPlan, CVPixelBufferPool, CVPixelBuffer, vImage_ARGBToYpCbCr)? in
            guard let plan, let pool, let scratch else { return nil }
            return (plan, pool, scratch, conversion)
        }) else { throw MediaError.Internal }
        var output: CVPixelBuffer?
        let aux = [kCVPixelBufferPoolAllocationThresholdKey: Int(plan.poolSize)] as CFDictionary
        let status = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(nil, pool, aux, &output)
        if status == kCVReturnWouldExceedAllocationThreshold { return .poolExhausted }
        guard status == kCVReturnSuccess, let canvas = output else { throw MediaError.OutOfResources }
        var images: [(CanvasTile, CVPixelBuffer)] = []
        for tile in plan.tiles {
            guard let member = members.first(where: { $0.member == tile.member }),
                  let entry = surfaces.entry(member.surface) else { throw MediaError.Internal }
            images.append((tile, entry.image))
        }
        try Self.composeBGRA(into: scratch, tiles: images)
        try Self.convert(scratch, to: canvas, info: info)
        CanvasColor.attach(plan.color, to: canvas)
        let id = surfaces.insert(canvas, h0: 0, h1: 0)
        return .composed(surface: SurfaceInfo(
            id: id, kind: .ioSurface, format: .nv12, width: plan.width, height: plan.height, color: plan.color,
            captureNs: MediaClock.now(), sourcePresentNs: nil, dirty: nil, rotation: .none, fence: .none))
    }

    public func release(surface: UInt64) { _ = surfaces.remove(surface) }

    /// The BGRA tile composition: opaque black, then each member's rows at its tile.
    public static func composeBGRA(into canvas: CVPixelBuffer, tiles: [(CanvasTile, CVPixelBuffer)]) throws {
        guard CVPixelBufferLockBaseAddress(canvas, []) == kCVReturnSuccess else { throw MediaError.Internal }
        defer { CVPixelBufferUnlockBaseAddress(canvas, []) }
        guard let base = CVPixelBufferGetBaseAddress(canvas) else { throw MediaError.Internal }
        let rowBytes = CVPixelBufferGetBytesPerRow(canvas)
        let width = CVPixelBufferGetWidth(canvas), height = CVPixelBufferGetHeight(canvas)
        var black: UInt32 = 0xFF00_0000
        for row in 0..<height { memset_pattern4(base + row * rowBytes, &black, width * 4) }
        for (tile, source) in tiles {
            guard CVPixelBufferGetPixelFormatType(source) == kCVPixelFormatType_32BGRA,
                  CVPixelBufferLockBaseAddress(source, .readOnly) == kCVReturnSuccess else { continue }
            if let from = CVPixelBufferGetBaseAddress(source) {
                let sourceRow = CVPixelBufferGetBytesPerRow(source)
                let columns = min(Int(tile.rect.width), CVPixelBufferGetWidth(source))
                for row in 0..<min(Int(tile.rect.height), CVPixelBufferGetHeight(source)) {
                    memcpy(base + (Int(tile.rect.y) + row) * rowBytes + Int(tile.rect.x) * 4, from + row * sourceRow, columns * 4)
                }
            }
            CVPixelBufferUnlockBaseAddress(source, .readOnly)
        }
    }

    public static func convert(_ bgra: CVPixelBuffer, to nv12: CVPixelBuffer, info: vImage_ARGBToYpCbCr) throws {
        guard CVPixelBufferLockBaseAddress(bgra, .readOnly) == kCVReturnSuccess else { throw MediaError.Internal }
        defer { CVPixelBufferUnlockBaseAddress(bgra, .readOnly) }
        guard CVPixelBufferLockBaseAddress(nv12, []) == kCVReturnSuccess else { throw MediaError.Internal }
        defer { CVPixelBufferUnlockBaseAddress(nv12, []) }
        let width = vImagePixelCount(CVPixelBufferGetWidth(bgra)), height = vImagePixelCount(CVPixelBufferGetHeight(bgra))
        var source = vImage_Buffer(data: CVPixelBufferGetBaseAddress(bgra), height: height, width: width,
                                   rowBytes: CVPixelBufferGetBytesPerRow(bgra))
        var luma = vImage_Buffer(data: CVPixelBufferGetBaseAddressOfPlane(nv12, 0), height: height, width: width,
                                 rowBytes: CVPixelBufferGetBytesPerRowOfPlane(nv12, 0))
        var chroma = vImage_Buffer(data: CVPixelBufferGetBaseAddressOfPlane(nv12, 1), height: height / 2, width: width / 2,
                                   rowBytes: CVPixelBufferGetBytesPerRowOfPlane(nv12, 1))
        var info = info
        // BGRA in memory is ARGB with the channels permuted: A=3, R=2, G=1, B=0.
        let permute: [UInt8] = [3, 2, 1, 0]
        guard vImageConvert_ARGB8888To420Yp8_CbCr8(&source, &luma, &chroma, &info, permute,
                                                   vImage_Flags(kvImageNoFlags)) == kvImageNoError else {
            throw MediaError.Internal
        }
    }
}
#endif
