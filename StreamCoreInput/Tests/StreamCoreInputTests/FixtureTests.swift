// Runs part of protocol/input/fixtures through the typed Swift bindings; the
// complete set runs in Rust (crates/streamcore-input/tests/conformance.rs).
import Foundation
import Testing
@testable import StreamCoreInput

private let fixtures = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    .deletingLastPathComponent().appendingPathComponent("protocol/input/fixtures")

private func load(_ name: String) throws -> [String: Any] {
    let data = try Data(contentsOf: fixtures.appendingPathComponent(name))
    return try JSONSerialization.jsonObject(with: data) as! [String: Any]
}

private func text(_ value: Any) throws -> String {
    String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed]), as: UTF8.self)
}

private func same(_ a: String, _ b: String) throws -> Bool {
    let x = try JSONSerialization.jsonObject(with: Data(a.utf8), options: [.fragmentsAllowed]) as AnyObject
    let y = try JSONSerialization.jsonObject(with: Data(b.utf8), options: [.fragmentsAllowed]) as AnyObject
    return x.isEqual(y)
}

@Test func keyShortcutsAndGesturesDecodeStrictly() throws {
    let doc = try load("wire-messages.json")
    for case let c as [String: Any] in doc["cases"] as! [Any] {
        // A string is the body's text (the duplicate-key cases).
        let body = try (c["json"] as? String) ?? text(c["json"]!)
        let canonical = try text(c["canonical"] ?? c["json"]!)
        switch c["type"] as! String {
        case "keyShortcut": #expect(try same(encodeKeyShortcut(message: decodeKeyShortcut(json: body)), canonical))
        case "gesture": #expect(try same(encodeGesture(message: decodeGesture(json: body)), canonical))
        case "text": #expect(try same(encodeText(message: decodeText(json: body)), canonical))
        default: break
        }
    }
    for case let c as [String: Any] in doc["invalid"] as! [Any] {
        let body = try text(c["json"]!)
        switch c["type"] as! String {
        case "keyShortcut": #expect(throws: StreamInputError.self) { try decodeKeyShortcut(json: body) }
        case "gesture": #expect(throws: StreamInputError.self) { try decodeGesture(json: body) }
        default: break
        }
    }
}

@Test func latchRoutesMatchPadMac() throws {
    let doc = try load("latch-merge.json")
    for case let c as [String: Any] in doc["merge"] as! [Any] {
        let host: HostOs = c["host"] as! String == "windows" ? .windows : .mac
        #expect(keyLatchMerge(requested: c["requested"] as! UInt8, latched: c["latched"] as! UInt8,
                              held: c["held"] as! UInt8, host: host) == c["expect"] as! UInt8)
    }
    for case let c as [String: Any] in doc["route"] as! [Any] {
        let host: HostOs = c["host"] as! String == "windows" ? .windows : .mac
        let route = keyLatchRoute(text: c["text"] as! String, latched: c["latched"] as! UInt8,
                                  held: c["held"] as! UInt8, host: host)
        let expect = c["expect"] as! [String: Any]
        switch route {
        case .text(let message): #expect(message.text == expect["text"] as? String)
        case .shortcuts(let messages):
            let encoded = "[" + messages.map(encodeKeyShortcut).joined(separator: ",") + "]"
            #expect(try same(encoded, text(expect["shortcuts"]!)))
        }
    }
}

@Test func jsonTableAndPresets() throws {
    // The generic path PadWin uses (C ABI) and fixture runners can use.
    let api = JsonApi()
    let doc = try load("modifiers.json")
    for case let c as [String: Any] in doc["resolve"] as! [Any] {
        for host in ["mac", "windows"] {
            let args = try text(["stored": c["stored"]!, "host": host])
            #expect(try api.call(method: "modifiers.resolve", args: args) == String(describing: c[host]!))
        }
    }
    #expect(PresetLibrary.builtin().presetFor(appKey: "C:\\Apps\\Krita\\bin\\krita.exe")?.preset == "krita")
}

@Test func vocabulariesAndConstants() throws {
    // Typed views over the stored strings: unknown values stay strings and read as nil.
    #expect(actionTypeFromName(name: "none") == ActionType.noAction)
    #expect(actionTypeFromName(name: "macro") == nil)
    #expect(actionTypeName(value: .noAction) == "none")
    #expect(twoFingerModeFromName(name: "zoomView") == .zoomView)
    #expect(gestureDeliveryFromName(name: "haptic") == nil)
    let profile = try decodeProfile(json: #"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"gestures":{"twoFinger":"orbit"}}"#)
    #expect(profile.gestures?.twoFinger == "orbit")
    #expect(try same(encodeProfile(profile: profile), #"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"gestures":{"twoFinger":"orbit"}}"#))
    #expect(migratedUpdatedAt() == "2000-01-02T00:00:00Z")
    #expect(presetUpdatedAt() == "2000-01-01T00:00:00Z")
    #expect(capabilityNames().contains(capabilityPenV2()))
    #expect(noticeCodes().contains(noticeKeyUnavailable()))
}

@Test func hardwareKeys() {
    #expect(hardwareKeyForHost(hid: hidLeftCommand(), stored: 8, host: .windows) == HostKey(hidUsage: hidLeftControl(), modifiers: 2))
    #expect(hardwareKeyForHost(hid: 0x06, stored: 8, host: .mac) == HostKey(hidUsage: 0x06, modifiers: 8))
    #expect(usKeyForHid(hid: 0x1D, shift: false) == "z")
    #expect(usKeyForHid(hid: 0x1E, shift: true) == "!")
    #expect(namedKeyForHid(hid: 0x29) == .escape)
    #expect(namedKeyForHid(hid: 0x04) == nil)
    #expect(isModifierHid(hid: hidRightShift()))
    let latch = LatchState()
    let packet = latch.keyPacket(packet: KeyPacket(isDown: true, hidUsage: hidLeftCommand(), modifiers: 8), host: .windows)
    #expect(packet.hidUsage == hidLeftControl() && packet.modifiers == 2)
}

@Test func perHostShortcutsAndPan() throws {
    let profile = try decodeProfile(json: #"{"schema":1,"appKey":"x","name":"X","updatedAt":"2026-01-01T00:00:00Z","groups":[],"gestures":{"delivery":"shortcuts","pan":"middleDrag"},"fallbacks":{"zoomIn":{"key":{"char":"+","hid":46},"modifiers":8,"hosts":{"windows":{"key":{"char":"=","hid":46},"modifiers":8}}}}}"#)
    let zoomIn = try #require(profile.fallbacks?.zoomIn)
    // `hosts` defaults to nil, so code written before it still builds.
    let plain = Shortcut(key: zoomIn.key, modifiers: 8, extra: nil)
    #expect(plain.hosts == nil && !shortcutHasHostOverrides(shortcut: plain))
    #expect(shortcutHasHostOverrides(shortcut: zoomIn))
    #expect(shortcutForHost(shortcut: zoomIn, host: .windows).key.character == "=")
    #expect(shortcutForHost(shortcut: zoomIn, host: .mac) == plain)
    #expect(resolveShortcut(shortcut: zoomIn, host: .windows).key.character == "=")
    let edit = Shortcut(key: profile.fallbacks!.zoomIn!.hosts!.windows!.key, modifiers: 9, extra: nil)
    let edited = shortcutWithHostEdit(shortcut: zoomIn, edited: edit, host: .windows)
    #expect(edited.key.character == "+" && edited.hosts?.windows?.modifiers == 9)
    #expect(panDeliveryFromName(name: "middleDrag") == .middleDrag)
    #expect(profileDefaults(profile: profile).pan == "middleDrag")
    let router = TwoFingerRouter()
    let began = try decodeGesture(json: #"{"kind":"twoFinger","phase":"began","dx":5,"dy":0}"#)
    let out = router.handle(message: began, delivery: profile.gestures?.delivery, pan: profile.gestures?.pan,
                            fallbacks: profile.fallbacks, panScale: 1, host: .windows)
    #expect(out == [.panDrag(via: .middleDrag, dx: 5, dy: 0, phase: .began)])
}

@Test func arraysAreNeverObjects() {
    // The typed decoders share the core's one decoding path with the JSON table.
    #expect(throws: StreamInputError.self) { try decodeText(json: #"["hi", 0]"#) }
    #expect(throws: StreamInputError.self) { try decodeSource(json: #"["display"]"#) }
    #expect(throws: StreamInputError.self) { try decodeAppList(json: #"{"apps": [["org.krita", "Krita"]]}"#) }
    #expect(throws: StreamInputError.self) { try decodeKeyShortcut(json: #"{"key": ["z"], "modifiers": 8}"#) }
    #expect((try? decodeServerNotices(json: #"[["display.asleep"], {"code": "computer.locked"}]"#))?.map(\.code) == ["computer.locked"])
}
