// Smoke test for the StreamCoreInput Swift bindings against the real library: the
// XCFramework (`swift run StreamCoreInputSmoke`, from scripts/build-streamcore-input-apple.sh),
// or compiled together with generated bindings against the debug dylib.
import Foundation
#if canImport(StreamCoreInput)
import StreamCoreInput
#endif

func check(_ condition: Bool, _ message: String) {
    if !condition {
        FileHandle.standardError.write(Data("FAIL: \(message)\n".utf8))
        exit(1)
    }
}

// Profiles: presets, last writer wins, the alias rule.
let store = ProfileStore()
check(store.resolve(appKey: "krita.exe")?.preset == "krita", "krita.exe resolves to the Krita preset")
let mine = try decodeProfile(json: """
{"schema": 1, "appKey": "org.krita", "name": "My Krita", "updatedAt": "2026-05-01T00:00:00Z", "groups": []}
""")
let outcome = store.upsert(profile: mine)
check(outcome.result == .accepted && outcome.changed && outcome.revision == 1, "upsert accepted")
check(store.resolve(appKey: "ORG.KRITA")?.name == "My Krita", "user profile wins")
check(store.resolve(appKey: "krita.exe")?.preset == "krita", "preset still covers krita.exe")
let saved = store.saveJson()
let reloaded = ProfileStore()
let report = try reloaded.loadJson(json: saved)
check(report.loaded == 1 && reloaded.revision() == 1, "save and load")
let deleted = store.delete(appKey: "org.krita")
check(deleted.removed && deleted.revealedPreset == "krita" && deleted.sendSnapshot, "delete reveals the preset")

// Default actions.
check(effectiveAction(profile: mine, slot: .tap4).pad == "toggleControls", "tap4 defaults to toggle controls")
check(pencilGestureResponse(profile: mine, gesture: .squeeze, system: .switchEraser) == .eraserSwap, "follows the system")

// Two fingers: spread to twice the distance, then lift.
let tracker = TwoFingerTracker()
let began = tracker.touches(x1: 0.25, y1: 0.5, x2: 0.75, y2: 0.5, width: 400, height: 200)
check(began?.phase == .began, "began")
let spread = tracker.touches(x1: 0.0, y1: 0.5, x2: 1.0, y2: 0.5, width: 400, height: 200)
check(abs((spread?.scale ?? 0) - 1.0) < 1e-9, "scale delta 1.0")
check(tracker.end()?.phase == .ended, "ended")
let wire = encodeGesture(message: spread!)
check(try decodeGesture(json: wire).kind == .twoFinger, "gesture round trip")

// Latches and typing.
let latch = LatchState()
_ = latch.toggle(modifiers: 8)
let routed = latch.routeText(text: "zS", replaceBefore: 0, host: .windows, capabilities: hostCapabilities(names: capabilityNames()))
check(routed.messages.count == 2, "two shortcut taps")
if case let .keyShortcut(message) = routed.messages[1] {
    check(message.modifiers == 3 && message.key.character == "s", "Ctrl+Shift+s on Windows")
} else {
    check(false, "keyShortcut expected")
}
check(redoShortcut(appKey: "notepad.exe", host: .windows, profile: nil).key.character == "y", "Ctrl+Y redo")

// The JSON method table (what PadWin calls through the C ABI).
let api = JsonApi()
check(try api.call(method: "modifiers.resolve", args: #"{"stored": 24, "host": "windows"}"#) == "10", "json api")
do {
    _ = try decodeKeyShortcut(json: #"{"key": {"char": "z"}, "modifiers": 16}"#)
    check(false, "platformKey bit must be rejected on the wire")
} catch StreamInputError.InvalidMessage {
}

print("StreamCoreInput Swift smoke test passed (input model \(inputModelVersion()))")
