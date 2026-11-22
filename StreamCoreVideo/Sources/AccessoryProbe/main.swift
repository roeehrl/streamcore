// A probe for the accessory path on real hardware (macOS only):
//
//   swift run AccessoryProbe [<vendor-hex> <product-hex>]
//
// It switches that cabled Android device into accessory mode (unless one is
// already in it), opens its bulk endpoints, runs the accessory link as the
// host, and serves one secure channel over a pipe: it measures the round trip
// and echoes every message. Trust is a throwaway pair of identities made
// here: the line it prints carries the client's identity and this host's
// certificate for the Android harness (`--es usb <line>`). It prints no
// serial numbers.
//
//   swift run AccessoryProbe --usbmux <port>
//
//   swift run AccessoryProbe --adb <port>
//
// serve the same echo channel through the device multiplexer (Apple devices) or a running
// adb server (Android) instead: a pipe is kept to <port> on each cabled device's loopback
// interface, where a test app listens (`UsbDeviceListener`) and dials back with the printed
// line (the Android harness: `--es usb <line> --ei usbport <port>`).
#if os(macOS)
import Foundation
import StreamCoreAppleHost
import StreamCoreVideo

func fail(_ text: String) -> Never {
    FileHandle.standardError.write(Data((text + "\n").utf8))
    exit(1)
}

// Progress shows at once when the output is a file or a pipe.
setvbuf(stdout, nil, _IONBF, 0)
let arguments = CommandLine.arguments
let identityStrings = AccessoryIdentity(
    manufacturer: "StreamCore", model: "Harness", summary: "StreamCore accessory probe",
    version: "1", uri: "https://example.invalid", serial: "probe")

/// Echoes the harness's test messages on `channel` until it ends.
func serve(_ channel: SecureChannel) async {
    print("channel over \(channel.transport())")
    var bytes = 0
    var messages = 0
    let started = Date()
    do {
        while true {
            let message = try await channel.receive()
            // Only the harness's test messages: the core's typed pongs are delivered
            // here too, and echoing one would break the clock protocol.
            guard message.first == 0xEE else { continue }
            bytes += message.count
            messages += 1
            try await channel.send(payload: message)
        }
    } catch {
        let seconds = Date().timeIntervalSince(started)
        print("channel ended after \(messages) messages, \(bytes) bytes echoed in \(String(format: "%.2f", seconds)) s: \(error)")
    }
}


// `clock.v1`: the round-trip probe.
let options = SessionOptions(capabilities: ["clock.v1"], required: ["clock.v1"])

if arguments.count >= 3, arguments[1] == "--usbmux" || arguments[1] == "--adb" {
    guard let port = UInt16(arguments[2]), port != 0 else { fail("usage: AccessoryProbe --usbmux|--adb <port>") }
    do {
        let host = try PeerIdentity.generate()
        let client = try PeerIdentity.generate()
        let line = try client.exportForSecureStorage().base64EncodedString() + "." + host.certificate().base64EncodedString()
        print("usb: \(line)")
        let listener = try await listenSecure(
            address: "127.0.0.1:0", identity: host, trustedPeers: [client.certificate()], options: options)
        let pipes = arguments[1] == "--adb"
            ? try UsbHostPipes.adb(listener: listener, sessionPort: port)
            : try UsbHostPipes.usbmux(listener: listener, program: "streamcore-probe", sessionPort: port)
        print("keeping pipes to port \(port) on cabled devices")
        while true {
            do {
                let channel = try await listener.accept()
                await serve(channel)
            } catch ChannelError.ConnectionFailed {
                // A pipe whose device side did not finish the handshake (the test app
                // is not running): the next one may.
                continue
            }
        }
        _ = pipes
    } catch {
        fail("failed: \(error)")
    }
}

// With ids, that device is switched (a device already in accessory mode takes the request
// again and announces the new identity); without, a device already in accessory mode is used.
if arguments.count >= 3 {
    guard let vendor = Int(arguments[1], radix: 16), let product = Int(arguments[2], radix: 16) else {
        fail("usage: AccessoryProbe [<vendor-hex> <product-hex>]")
    }
    guard let device = AndroidAccessory.devices().first(where: { $0.vendor == vendor && $0.product == product }) else {
        fail("no such device attached")
    }
    do {
        try AndroidAccessory.switchToAccessory(device, identity: identityStrings)
    } catch {
        fail("switch failed: \(error)")
    }
    print("switch requested")
    // The device leaves the bus before it comes back.
    try? await Task.sleep(nanoseconds: 1_500_000_000)
} else if AndroidAccessory.accessories().isEmpty {
    fail("usage: AccessoryProbe [<vendor-hex> <product-hex>] (no device is in accessory mode)")
}

do {
    let accessory = try AndroidAccessory.waitForAccessory(seconds: 10)
    print("accessory product \(String(accessory.product, radix: 16))")
    let stream = try AndroidAccessory.open(accessory)
    let link = UsbAccessoryLink.start(io: stream, role: .host, device: "android")

    let host = try PeerIdentity.generate()
    let client = try PeerIdentity.generate()
    let line = try client.exportForSecureStorage().base64EncodedString() + "." + host.certificate().base64EncodedString()
    print("usb: \(line)")

    let listener = try await listenSecure(
        address: "127.0.0.1:0", identity: host, trustedPeers: [client.certificate()], options: options)
    while !link.isClosed() {
        print("opening a pipe…")
        let pipe: UsbPipeHandle
        do {
            pipe = try await link.openPipe(service: .session)
        } catch {
            print("open failed: \(error); retrying")
            try await Task.sleep(nanoseconds: 1_000_000_000)
            continue
        }
        let admission = try listener.acceptPipe(pipe: pipe)
        let outcome = await admission.outcome()
        print("pipe outcome: \(outcome)")
        guard outcome == .accepted else { continue }
        let channel = try await listener.accept()
        await serve(channel)
    }
    print("link closed")
} catch {
    fail("failed: \(error)")
}
#else
print("AccessoryProbe runs on macOS only")
#endif
