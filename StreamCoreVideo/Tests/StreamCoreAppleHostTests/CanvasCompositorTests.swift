#if os(macOS)
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import Metal
@preconcurrency import ScreenCaptureKit
import StreamCoreVideo
import Synchronization
import Testing
@testable import StreamCoreAppleHost

// MARK: - Pixels

private func bgra(width: Int, height: Int, color: (Double, Double, Double), primaries: CFString,
                  transfer: CFString = kCVImageBufferTransferFunction_sRGB) throws -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    try #require(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA,
        [kCVPixelBufferIOSurfacePropertiesKey: [:], kCVPixelBufferMetalCompatibilityKey: true] as CFDictionary,
        &buffer) == kCVReturnSuccess)
    let pixels = try #require(buffer)
    CVPixelBufferLockBaseAddress(pixels, [])
    let base = try #require(CVPixelBufferGetBaseAddress(pixels)?.assumingMemoryBound(to: UInt8.self))
    let rowBytes = CVPixelBufferGetBytesPerRow(pixels)
    let value = { (v: Double) in UInt8((min(max(v, 0), 1) * 255).rounded()) }
    for y in 0..<height {
        for x in 0..<width {
            let o = y * rowBytes + x * 4
            base[o] = value(color.2); base[o + 1] = value(color.1); base[o + 2] = value(color.0); base[o + 3] = 255
        }
    }
    CVPixelBufferUnlockBaseAddress(pixels, [])
    CVBufferSetAttachment(pixels, kCVImageBufferColorPrimariesKey, primaries, .shouldPropagate)
    CVBufferSetAttachment(pixels, kCVImageBufferTransferFunctionKey, transfer, .shouldPropagate)
    return pixels
}

/// RGB (0–1, gamma-encoded) of an NV12 canvas pixel, BT.709 matrix, limited range.
private func rgb(nv12 buffer: CVPixelBuffer, x: Int, y: Int) -> [Double] {
    CVPixelBufferLockBaseAddress(buffer, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
    let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)!.assumingMemoryBound(to: UInt8.self)
    let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)!.assumingMemoryBound(to: UInt8.self)
    let yv = (Double(luma[y * CVPixelBufferGetBytesPerRowOfPlane(buffer, 0) + x]) - 16) / 219
    let c = (y / 2) * CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) + (x / 2) * 2
    let cb = (Double(chroma[c]) - 128) / 224, cr = (Double(chroma[c + 1]) - 128) / 224
    let kr = 0.2126, kb = 0.0722, kg = 1 - kr - kb
    let r = yv + 2 * (1 - kr) * cr, b = yv + 2 * (1 - kb) * cb
    let g = (yv - kr * r - kb * b) / kg
    return [r, g, b].map { min(max($0, 0), 1) }
}

/// A Display P3 colour as ScreenCaptureKit delivers it in sRGB (the CPU path's members).
private func p3InSRGB(_ color: (Double, Double, Double)) throws -> (Double, Double, Double) {
    let p3 = try #require(CGColorSpace(name: CGColorSpace.displayP3)), srgb = try #require(CGColorSpace(name: CGColorSpace.sRGB))
    let source = try #require(CGColor(colorSpace: p3, components: [color.0, color.1, color.2, 1]))
    let converted = try #require(source.converted(to: srgb, intent: .relativeColorimetric, options: nil)?.components)
    return (min(max(converted[0], 0), 1), min(max(converted[1], 0), 1), min(max(converted[2], 0), 1))
}

private let srgbCanvas = ColorTags(primaries: .bt709, transfer: .srgb, matrix: .bt709, fullRange: false)

/// Three members in an L of 64 × 48 tiles: a gap at the bottom right of a 128 × 96 canvas.
private func plan(width: UInt32, height: UInt32, tiles: [CanvasTile], pool: UInt8 = 5) -> CanvasPlan {
    CanvasPlan(width: width, height: height, tiles: tiles, color: srgbCanvas, poolSize: pool)
}

private func lTiles() -> [CanvasTile] {
    [CanvasTile(member: 0, rect: Rect(x: 0, y: 0, width: 64, height: 48)),
     CanvasTile(member: 1, rect: Rect(x: 64, y: 0, width: 64, height: 48)),
     CanvasTile(member: 2, rect: Rect(x: 0, y: 48, width: 64, height: 48))]
}

private func wait(_ fence: GpuFence, on event: MTLSharedEvent) throws {
    guard case .mtlSharedEvent(let handle, let value) = fence else { Issue.record("no Metal fence: \(fence)"); return }
    #expect(handle == 0, "the compositor's own in-process event")
    let deadline = Date().addingTimeInterval(2)
    while event.signaledValue < value, Date() < deadline { usleep(200) }
    #expect(event.signaledValue >= value, "the MTLSharedEvent completes the composite")
}

// MARK: - Metal compositor

@Suite(.serialized) struct MetalCanvasCompositorTests {
    private let colors: [(Double, Double, Double)] = [(0.9, 0.1, 0.1), (0.1, 0.8, 0.2), (0.15, 0.25, 0.9)]

    /// Composes `sources` with both compositors (the CPU one gets `cpuSources`) and returns
    /// each canvas.
    private func compose(_ sources: [CVPixelBuffer], cpuSources: [CVPixelBuffer]) throws -> (metal: CVPixelBuffer, cpu: CVPixelBuffer) {
        let surfaces = SurfaceStore()
        let metal = try MetalCanvasCompositor(surfaces: surfaces)
        let cpu = CPUCanvasCompositor(surfaces: surfaces)
        try metal.configure(plan: plan(width: 128, height: 96, tiles: lTiles()))
        try cpu.configure(plan: plan(width: 128, height: 96, tiles: lTiles()))
        let metalMembers = sources.enumerated().map { MemberSurface(member: UInt8($0.offset), surface: surfaces.insert($0.element, h0: 0, h1: 0)) }
        let cpuMembers = cpuSources.enumerated().map { MemberSurface(member: UInt8($0.offset), surface: surfaces.insert($0.element, h0: 0, h1: 0)) }
        guard case .composed(let m) = try metal.compose(members: metalMembers),
              case .composed(let c) = try cpu.compose(members: cpuMembers) else {
            throw MediaError.Internal
        }
        try wait(m.fence, on: metal.event)
        #expect(c.fence == GpuFence.none, "the CPU composite is complete when compose returns")
        #expect(m.format == .nv12 && m.width == 128 && m.height == 96 && m.color == srgbCanvas)
        let metalImage = try #require(surfaces.entry(m.id)?.image)
        let cpuImage = try #require(surfaces.entry(c.id)?.image)
        #expect(CVPixelBufferGetPixelFormatType(metalImage) == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        #expect(SurfaceColor.tags(of: metalImage).primaries == .bt709)
        return (metalImage, cpuImage)
    }

    /// Both compositors (Metal, and the CPU's BGRA tile copy) place every member at its
    /// tile and leave gaps opaque black.
    @Test func tilesLandInPlaceGapsAreBlackAndTheOutputIsNV12() throws {
        let sources = try colors.map { try bgra(width: 64, height: 48, color: $0, primaries: kCVImageBufferColorPrimaries_ITU_R_709_2) }
        let (metal, cpu) = try compose(sources, cpuSources: sources)
        #expect(CVPixelBufferGetPixelFormatType(cpu) == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        for canvas in [metal, cpu] { for (tile, color) in zip(lTiles(), colors) {
            // Every corner of a tile, inset by a chroma block, is the member's colour.
            let xs = [Int(tile.rect.x) + 2, Int(tile.rect.x + tile.rect.width) - 3]
            let ys = [Int(tile.rect.y) + 2, Int(tile.rect.y + tile.rect.height) - 3]
            for x in xs { for y in ys {
                let actual = rgb(nv12: canvas, x: x, y: y)
                #expect(zip(actual, [color.0, color.1, color.2]).allSatisfy { abs($0 - $1) <= 0.12 }, "\(x),\(y): \(actual)")
            } }
        }
        for (x, y) in [(70, 50), (127, 95), (100, 70)] {
            #expect(rgb(nv12: canvas, x: x, y: y).allSatisfy { $0 <= 0.03 }, "gap at \(x),\(y)")
        } }
    }

    /// The CPU compositor's pool is bounded: with every canvas held it reports exhaustion,
    /// and composes again once one is released.
    @Test func theCPUCompositorsPoolIsBoundedAndReusesAReleasedCanvas() throws {
        let surfaces = SurfaceStore()
        let cpu = CPUCanvasCompositor(surfaces: surfaces)
        try cpu.configure(plan: plan(width: 128, height: 96, tiles: lTiles(), pool: 3))
        let members = try colors.enumerated().map {
            MemberSurface(member: UInt8($0.offset), surface: surfaces.insert(
                try bgra(width: 64, height: 48, color: $0.element, primaries: kCVImageBufferColorPrimaries_ITU_R_709_2), h0: 0, h1: 0))
        }
        var held: [SurfaceInfo] = []
        for _ in 0..<3 {
            guard case .composed(let info) = try cpu.compose(members: members) else { Issue.record("exhausted early"); return }
            held.append(info)
        }
        #expect(try cpu.compose(members: members) == .poolExhausted)
        cpu.release(surface: held[0].id)
        guard case .composed = try cpu.compose(members: members) else { Issue.record("still exhausted"); return }
    }

    /// The 0.12 RGB colour gate (9.6) against the CPU path: BT.709 (sRGB) members as they
    /// are, and Display P3 members, which the CPU path gets from ScreenCaptureKit already
    /// in sRGB while the Metal path maps the gamut in its shader.
    @Test(arguments: [false, true]) func colourMatchesTheCPUPathWithinTheGate(p3: Bool) throws {
        let palette: [(Double, Double, Double)] = p3
            ? [(0.85, 0.2, 0.15), (0.3, 0.75, 0.35), (0.25, 0.3, 0.8)] : colors
        let primaries = p3 ? kCVImageBufferColorPrimaries_P3_D65 : kCVImageBufferColorPrimaries_ITU_R_709_2
        let sources = try palette.map { try bgra(width: 64, height: 48, color: $0, primaries: primaries) }
        let cpuSources = try palette.map {
            try bgra(width: 64, height: 48, color: p3 ? try p3InSRGB($0) : $0, primaries: kCVImageBufferColorPrimaries_ITU_R_709_2)
        }
        let (metal, cpu) = try compose(sources, cpuSources: cpuSources)
        var worst = 0.0
        for tile in lTiles() {
            let x = Int(tile.rect.x + tile.rect.width / 2), y = Int(tile.rect.y + tile.rect.height / 2)
            let a = rgb(nv12: metal, x: x, y: y), b = rgb(nv12: cpu, x: x, y: y)
            let diff = zip(a, b).map { abs($0 - $1) }.max() ?? 1
            worst = max(worst, diff)
            #expect(diff <= 0.12, "member \(tile.member): metal \(a) cpu \(b)")
        }
        print("canvas colour gate (\(p3 ? "Display P3" : "BT.709") members): worst RGB difference metal vs cpu \(String(format: "%.4f", worst))")
    }

    @Test func anExhaustedPoolComposesAgainOnceASurfaceIsReleased() throws {
        let surfaces = SurfaceStore()
        let metal = try MetalCanvasCompositor(surfaces: surfaces)
        try metal.configure(plan: plan(width: 128, height: 96, tiles: lTiles(), pool: 3))
        let members = try colors.enumerated().map {
            MemberSurface(member: UInt8($0.offset), surface: surfaces.insert(
                try bgra(width: 64, height: 48, color: $0.element, primaries: kCVImageBufferColorPrimaries_ITU_R_709_2), h0: 0, h1: 0))
        }
        var held: [SurfaceInfo] = []
        for _ in 0..<3 {
            guard case .composed(let info) = try metal.compose(members: members) else { Issue.record("exhausted early"); return }
            held.append(info)
        }
        #expect(try metal.compose(members: members) == .poolExhausted)
        for info in held { try wait(info.fence, on: metal.event) }
        metal.release(surface: held[0].id)
        // The pool gets the surface back once the GPU let go of it (its completion handler).
        var result = try metal.compose(members: members)
        let deadline = Date().addingTimeInterval(1)
        while result == .poolExhausted, Date() < deadline { usleep(1_000); result = try metal.compose(members: members) }
        guard case .composed = result else { Issue.record("still exhausted"); return }
    }
}

struct CanvasRotationTests {
    /// A display's rotation is part of its layout; a mirrored one is gone.
    @Test func observedLayoutsCarryTheDisplayRotationAndMirrorsAreGone() {
        let observed = Mutex<[UInt32: DisplayLayoutObservation]>([
            1: DisplayLayoutObservation(bounds: CGRect(x: 0, y: 0, width: 720, height: 1280), scale: 2, rotation: 90,
                                        mirrorOf: nil, pixelWidth: 1440, pixelHeight: 2560),
            2: DisplayLayoutObservation(bounds: CGRect(x: 720, y: 0, width: 800, height: 600), scale: 1, rotation: 0,
                                        mirrorOf: 1, pixelWidth: 800, pixelHeight: 600),
        ])
        let backend = CanvasCaptureBackend(surfaces: SurfaceStore(), wideGamut: false, trace: nil,
                                           observe: { display in observed.withLock { $0[display] } },
                                           makeStream: { _, _, _ in throw MediaError.Unsupported })
        let turned = backend.observeLayout(display: 1)
        #expect(turned == DisplayLayout(displayId: 1, pixelWidth: 1440, pixelHeight: 2560, scale: 2, originX: 0, originY: 0,
                                        rotation: .rotate90))
        #expect(backend.observeLayout(display: 2) == nil, "a mirror has no layout of its own")
        #expect(backend.observeLayout(display: 3) == nil)
        #expect([0, 90, 180, 270, -90, 450].map(CanvasCaptureBackend.rotation(degrees:))
                == [.identity, .rotate90, .rotate180, .rotate270, .rotate270, .rotate90])
        #expect([CGImagePropertyOrientation.up, .right, .down, .left, .upMirrored].map(CanvasCaptureBackend.turn)
                == [Rotation.none, .rotate90, .rotate180, .rotate270, .none])
    }

    /// A member stored sideways (its frame says it needs a clockwise quarter turn) lands
    /// upright in its tile: the stored image's left half becomes the tile's top.
    @Test func theMetalCompositorTurnsAMemberThatCarriesAnOrientation() throws {
        let surfaces = SurfaceStore()
        let metal = try MetalCanvasCompositor(surfaces: surfaces)
        let tiles = [CanvasTile(member: 0, rect: Rect(x: 0, y: 0, width: 64, height: 48)),
                     CanvasTile(member: 1, rect: Rect(x: 64, y: 0, width: 64, height: 48))]
        try metal.configure(plan: CanvasPlan(width: 128, height: 48, tiles: tiles, color: srgbCanvas, poolSize: 5))
        // Stored 48 × 64: left half red, right half blue.
        let sideways = try bgra(width: 48, height: 64, color: (0.9, 0.1, 0.1), primaries: kCVImageBufferColorPrimaries_ITU_R_709_2)
        CVPixelBufferLockBaseAddress(sideways, [])
        let base = try #require(CVPixelBufferGetBaseAddress(sideways)?.assumingMemoryBound(to: UInt8.self))
        for y in 0..<64 { for x in 24..<48 {
            let o = y * CVPixelBufferGetBytesPerRow(sideways) + x * 4
            base[o] = 230; base[o + 1] = 25; base[o + 2] = 25
        } }
        CVPixelBufferUnlockBaseAddress(sideways, [])
        let upright = try bgra(width: 64, height: 48, color: (0.1, 0.8, 0.2), primaries: kCVImageBufferColorPrimaries_ITU_R_709_2)
        let members = [MemberSurface(member: 0, surface: surfaces.insert(sideways, h0: 0, h1: 0, rotation: .rotate90)),
                       MemberSurface(member: 1, surface: surfaces.insert(upright, h0: 0, h1: 0))]
        guard case .composed(let info) = try metal.compose(members: members) else { Issue.record("not composed"); return }
        try wait(info.fence, on: metal.event)
        let canvas = try #require(surfaces.entry(info.id)?.image)
        let top = rgb(nv12: canvas, x: 32, y: 8), bottom = rgb(nv12: canvas, x: 32, y: 40)
        #expect(top[0] > 0.7 && top[2] < 0.3, "the stored left half is the top: \(top)")
        #expect(bottom[2] > 0.7 && bottom[0] < 0.3, "the stored right half is the bottom: \(bottom)")
        let other = rgb(nv12: canvas, x: 96, y: 24)
        #expect(other[1] > 0.6, "an upright member is untouched: \(other)")
    }
}
#endif
