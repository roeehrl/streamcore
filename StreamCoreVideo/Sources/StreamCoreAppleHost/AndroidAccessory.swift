// The macOS side of an Android accessory connection (design 8.4): switches a
// cabled Android device into accessory mode and gives its bulk endpoints to
// Rust as a `ByteDuplex`. Nothing here is identity: whatever is on the cable
// can answer these requests, and the handshake on each pipe decides trust.
#if os(macOS)
import Foundation
import IOKit
import IOKit.usb
import IOUSBHost
import StreamCoreVideo

/// The six strings an accessory announces. Android starts the app whose
/// accessory filter matches `manufacturer`, `model` and `version`.
public struct AccessoryIdentity: Sendable {
    public var manufacturer: String
    public var model: String
    public var summary: String
    public var version: String
    public var uri: String
    public var serial: String

    public init(manufacturer: String, model: String, summary: String, version: String, uri: String, serial: String) {
        self.manufacturer = manufacturer
        self.model = model
        self.summary = summary
        self.version = version
        self.uri = uri
        self.serial = serial
    }
}

public enum AccessoryError: Error, Equatable {
    /// No such device is attached.
    case notFound
    /// The device does not speak the accessory protocol.
    case unsupported
    /// A USB request failed.
    case usb(String)
    /// The device did not come back as an accessory in time.
    case timeout
    /// The accessory has no pair of bulk endpoints.
    case endpoints
}

/// A cabled USB device, by its registry entry. Local routing only.
public struct UsbDeviceRef: Sendable, Equatable {
    public let vendor: Int
    public let product: Int
    /// The registry entry's id: stable while the device stays attached.
    public let entry: UInt64
}

public enum AndroidAccessory {
    /// Google's vendor id, which every device in accessory mode reports.
    public static let vendor = 0x18D1
    /// Accessory, and accessory with the debug bridge.
    public static let products = [0x2D00, 0x2D01]

    /// The attached devices that are in accessory mode now.
    public static func accessories() -> [UsbDeviceRef] {
        devices().filter { $0.vendor == vendor && products.contains($0.product) }
    }

    /// Every attached USB device. Matching on the vendor alone finds nothing
    /// in the registry, so the caller filters.
    public static func devices() -> [UsbDeviceRef] {
        var iterator: io_iterator_t = 0
        guard IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("IOUSBHostDevice"), &iterator) == KERN_SUCCESS else {
            return []
        }
        defer { IOObjectRelease(iterator) }
        var found: [UsbDeviceRef] = []
        while case let service = IOIteratorNext(iterator), service != 0 {
            defer { IOObjectRelease(service) }
            var entry: UInt64 = 0
            guard IORegistryEntryGetRegistryEntryID(service, &entry) == KERN_SUCCESS,
                  let vendor = number(service, "idVendor"), let product = number(service, "idProduct") else { continue }
            found.append(UsbDeviceRef(vendor: vendor, product: product, entry: entry))
        }
        return found
    }

    /// Asks `device` to become an accessory announcing `identity`. The device
    /// then leaves the bus and comes back under [`vendor`]; wait for it with
    /// [`waitForAccessory`]. Android shows a prompt unless an installed app's
    /// accessory filter matches.
    public static func switchToAccessory(_ device: UsbDeviceRef, identity: AccessoryIdentity) throws {
        guard let service = service(entry: device.entry) else { throw AccessoryError.notFound }
        defer { IOObjectRelease(service) }
        let host: IOUSBHostDevice
        do {
            host = try IOUSBHostDevice(__ioService: service, options: [], queue: nil, interestHandler: nil)
        } catch {
            throw AccessoryError.usb("open: \(error.localizedDescription)")
        }
        defer { host.destroy() }
        let version = NSMutableData(length: 2)!
        guard (try? control(host, type: 0xC0, request: 51, index: 0, data: version)) == 2 else {
            throw AccessoryError.unsupported
        }
        let bytes = [UInt8](version as Data)
        guard Int(bytes[0]) | Int(bytes[1]) << 8 >= 1 else { throw AccessoryError.unsupported }
        let strings = [identity.manufacturer, identity.model, identity.summary, identity.version, identity.uri, identity.serial]
        do {
            for (index, text) in strings.enumerated() {
                let data = NSMutableData(data: Data(text.utf8) + [0])
                _ = try control(host, type: 0x40, request: 52, index: UInt16(index), data: data)
            }
            _ = try control(host, type: 0x40, request: 53, index: 0, data: nil)
        } catch {
            throw AccessoryError.usb("switch: \(error.localizedDescription)")
        }
    }

    /// The first device in accessory mode, waiting up to `seconds` for one.
    public static func waitForAccessory(seconds: Double) throws -> UsbDeviceRef {
        let deadline = Date().addingTimeInterval(seconds)
        while true {
            if let accessory = accessories().first { return accessory }
            guard Date() < deadline else { throw AccessoryError.timeout }
            Thread.sleep(forTimeInterval: 0.1)
        }
    }

    /// Opens the accessory interface's bulk endpoints as a byte stream.
    public static func open(_ accessory: UsbDeviceRef) throws -> AccessoryStream {
        guard let device = service(entry: accessory.entry) else { throw AccessoryError.notFound }
        defer { IOObjectRelease(device) }
        // The accessory interface is vendor-specific (255/255/0); the debug
        // bridge's, when present, has subclass 66.
        var children: io_iterator_t = 0
        guard IORegistryEntryGetChildIterator(device, kIOServicePlane, &children) == KERN_SUCCESS else {
            throw AccessoryError.endpoints
        }
        defer { IOObjectRelease(children) }
        while case let child = IOIteratorNext(children), child != 0 {
            defer { IOObjectRelease(child) }
            guard IOObjectConformsTo(child, "IOUSBHostInterface") != 0,
                  number(child, "bInterfaceClass") == 255, number(child, "bInterfaceSubClass") == 255,
                  number(child, "bInterfaceProtocol") == 0 else { continue }
            return try AccessoryStream(interface: child)
        }
        throw AccessoryError.endpoints
    }

    private static func control(_ device: IOUSBHostDevice, type: UInt8, request: UInt8, index: UInt16, data: NSMutableData?) throws -> Int {
        let setup = IOUSBDeviceRequest(bmRequestType: type, bRequest: request, wValue: 0, wIndex: index, wLength: UInt16(data?.length ?? 0))
        var done = 0
        try device.__send(setup, data: data, bytesTransferred: &done, completionTimeout: 2.0)
        return done
    }

    private static func number(_ service: io_service_t, _ key: String) -> Int? {
        (IORegistryEntryCreateCFProperty(service, key as CFString, kCFAllocatorDefault, 0)?.takeRetainedValue() as? NSNumber)?.intValue
    }

    private static func service(entry: UInt64) -> io_service_t? {
        let service = IOServiceGetMatchingService(kIOMainPortDefault, IORegistryEntryIDMatching(entry))
        return service == 0 ? nil : service
    }
}

/// An accessory's bulk endpoints as the blocking stream Rust reads and writes
/// (`ByteDuplex`). `shutdown` aborts both endpoints, which frees a blocked
/// call.
public final class AccessoryStream: ByteDuplex, @unchecked Sendable {
    private let interface: IOUSBHostInterface
    private let input: IOUSBHostPipe
    private let output: IOUSBHostPipe
    private let outputPacket: Int
    private let state = NSLock()
    private var closed = false
    /// The most bytes in one transfer to the device.
    static let transfer = 16 * 1024

    init(interface service: io_service_t) throws {
        do {
            interface = try IOUSBHostInterface(__ioService: service, options: [], queue: nil, interestHandler: nil)
        } catch {
            throw AccessoryError.usb("open interface: \(error.localizedDescription)")
        }
        var inAddress: Int?
        var outAddress: Int?
        var outPacket = 512
        let configuration = interface.configurationDescriptor
        let descriptor = interface.interfaceDescriptor
        var endpoint = IOUSBGetNextEndpointDescriptor(configuration, descriptor, nil)
        while let current = endpoint {
            let address = Int(current.pointee.bEndpointAddress)
            if current.pointee.bmAttributes & 0x03 == 0x02 {
                if address & 0x80 != 0 {
                    inAddress = inAddress ?? address
                } else if outAddress == nil {
                    outAddress = address
                    outPacket = max(1, Int(UInt16(littleEndian: current.pointee.wMaxPacketSize) & 0x07FF))
                }
            }
            endpoint = IOUSBGetNextEndpointDescriptor(configuration, descriptor, UnsafeRawPointer(current).assumingMemoryBound(to: IOUSBDescriptorHeader.self))
        }
        guard let inAddress, let outAddress else {
            interface.destroy()
            throw AccessoryError.endpoints
        }
        do {
            input = try interface.copyPipe(withAddress: inAddress)
            output = try interface.copyPipe(withAddress: outAddress)
        } catch {
            interface.destroy()
            throw AccessoryError.usb("endpoints: \(error.localizedDescription)")
        }
        outputPacket = outPacket
    }

    private var isClosed: Bool {
        state.lock()
        defer { state.unlock() }
        return closed
    }

    public func read(max: UInt32) throws -> Data {
        // A request ends on a short packet or a full buffer.
        let buffer = NSMutableData(length: Int(max))!
        while true {
            if isClosed { throw DuplexError.Closed }
            var done = 0
            do {
                try input.__sendIORequest(with: buffer, bytesTransferred: &done, completionTimeout: 0)
            } catch {
                throw isClosed ? DuplexError.Closed : DuplexError.Failed
            }
            // A zero-length packet carries nothing and is not the end.
            if done > 0 { return Data(bytes: buffer.bytes, count: done) }
        }
    }

    public func write(data: Data) throws {
        if isClosed { throw DuplexError.Closed }
        do {
            // Android's accessory driver hands an app one transfer per read, into a 16 KiB
            // buffer: a longer transfer would lose its tail.
            var offset = 0
            while offset < data.count {
                let count = min(AccessoryStream.transfer, data.count - offset)
                let chunk = NSMutableData(data: data.subdata(in: (data.startIndex + offset)..<(data.startIndex + offset + count)))
                var done = 0
                try output.__sendIORequest(with: chunk, bytesTransferred: &done, completionTimeout: 0)
                guard done == count else { throw DuplexError.Failed }
                if count < AccessoryStream.transfer && count % outputPacket == 0 {
                    // The device's read ends on a short packet or a full buffer: a
                    // shorter transfer of whole packets would wait for more.
                    try output.__sendIORequest(with: nil, bytesTransferred: &done, completionTimeout: 0)
                }
                offset += count
            }
        } catch {
            throw isClosed ? DuplexError.Closed : DuplexError.Failed
        }
    }

    public func shutdown() {
        state.lock()
        let first = !closed
        closed = true
        state.unlock()
        guard first else { return }
        try? input.__abort(with: .synchronous)
        try? output.__abort(with: .synchronous)
        interface.destroy()
    }
}
#endif
