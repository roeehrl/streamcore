package streamcore.video.harness

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.ParcelFileDescriptor
import android.util.Base64
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import uniffi.streamcore_transport.ByteDuplex
import uniffi.streamcore_transport.DuplexException
import uniffi.streamcore_transport.OperationCancellation
import uniffi.streamcore_transport.PeerIdentity
import uniffi.streamcore_transport.SecureChannel
import uniffi.streamcore_transport.SessionOptions
import uniffi.streamcore_transport.connectSecurePipe
import uniffi.streamcore_transport.Medium
import uniffi.streamcore_usb.LinkRole
import uniffi.streamcore_usb.UsbAccessoryLink
import uniffi.streamcore_usb.UsbDeviceListener

/**
 * An accessory connection as the blocking stream Rust reads and writes. Android hands an app
 * one file descriptor for both bulk endpoints.
 */
class AccessoryDuplex(private val descriptor: ParcelFileDescriptor) : ByteDuplex {
    private val input = FileInputStream(descriptor.fileDescriptor)
    private val output = FileOutputStream(descriptor.fileDescriptor)
    private val closed = AtomicBoolean(false)

    // The accessory driver returns whole transfers: a buffer smaller than one (16 KiB) loses
    // the rest of it.
    private val buffer = ByteArray(BUFFER)

    override fun read(max: UInt): ByteArray {
        if (closed.get()) throw DuplexException.Closed()
        while (true) {
            val read = try {
                input.read(buffer, 0, BUFFER)
            } catch (e: IOException) {
                // Unplugged, or shut down under the read.
                throw DuplexException.Closed()
            }
            if (read < 0) return ByteArray(0)
            // A zero-length packet carries nothing and is not the end.
            if (read > 0) return buffer.copyOf(read)
        }
    }

    override fun write(data: ByteArray) {
        if (closed.get()) throw DuplexException.Closed()
        try {
            output.write(data)
        } catch (e: IOException) {
            throw if (closed.get()) DuplexException.Closed() else DuplexException.Failed()
        }
    }

    override fun shutdown() {
        if (!closed.compareAndSet(false, true)) return
        runCatching { descriptor.close() }
    }

    private companion object {
        const val BUFFER = 16 * 1024
    }
}

/**
 * The device side of the macOS accessory probe (`--es usb <line>`, the line the probe
 * prints): dials the host over the accessory with the throwaway identity in the line, measures
 * the round trip, then echoes messages of every size up to [SWEEP] bytes and a bulk run.
 */
object AccessoryProbe {
    private const val SWEEP = 2_100
    private const val BULK_MESSAGES = 300
    private const val BULK_BYTES = 16_000

    /** The first byte of every test message. */
    private const val MARK = 0xEE.toByte()

    /** The round-trip probe needs typed pings. */
    private const val CLOCK = "clock.v1"
    private const val PERMISSION_ACTION = "streamcore.video.harness.ACCESSORY_PERMISSION"

    /** The next test message echoed back; the core's typed pongs arrive on the same call. */
    private suspend fun echo(channel: SecureChannel): ByteArray {
        while (true) {
            val message = channel.receive()
            if (message.firstOrNull() == MARK) return message
        }
    }

    fun accessory(context: Context): UsbAccessory? {
        val manager = context.getSystemService(Context.USB_SERVICE) as UsbManager
        return manager.accessoryList?.firstOrNull()
    }

    private suspend fun openAccessory(context: Context, log: (String) -> Unit): ParcelFileDescriptor? {
        val manager = context.getSystemService(Context.USB_SERVICE) as UsbManager
        val accessory = accessory(context) ?: return null.also { log("usb: no accessory attached") }
        if (!manager.hasPermission(accessory)) {
            // Started by hand, not by the attach: Android asks the user.
            log("usb: asking for the accessory; allow it on the device")
            val answer = PendingIntent.getBroadcast(
                context, 0, Intent(PERMISSION_ACTION).setPackage(context.packageName), PendingIntent.FLAG_IMMUTABLE,
            )
            manager.requestPermission(accessory, answer)
            for (attempt in 0 until 120) {
                if (manager.hasPermission(accessory)) break
                delay(500)
            }
            if (!manager.hasPermission(accessory)) return null.also { log("usb: no permission for the accessory") }
        }
        return manager.openAccessory(accessory) ?: null.also { log("usb: the accessory did not open") }
    }

    /**
     * Over the accessory, or with [adbPort] over a pipe the host opens through adb to that
     * loopback port.
     */
    suspend fun run(context: Context, line: String, adbPort: Int?, log: (String) -> Unit) {
        val parts = line.trim().split('.')
        if (parts.size != 2) return log("usb: not a probe line")
        val identity = PeerIdentity.restore(Base64.decode(parts[0], Base64.DEFAULT))
        val host = Base64.decode(parts[1], Base64.DEFAULT)
        var link: UsbAccessoryLink? = null
        try {
            val pipe = if (adbPort != null) {
                val listener = UsbDeviceListener.bind(listOf(adbPort.toUShort()), Medium.USB_ADB)
                log("usb: listening on ${listener.port()}")
                withTimeout(30_000) { listener.accept() }
            } else {
                val accessory = openAccessory(context, log) ?: return
                link = UsbAccessoryLink.start(AccessoryDuplex(accessory), LinkRole.DEVICE, "host")
                log("usb: waiting for the host's pipe")
                (withTimeout(20_000) { link.accept() } ?: return log("usb: the link ended")).pipe
            }
            val channel = connectSecurePipe(
                identity, host, pipe, SessionOptions(listOf(CLOCK), listOf(CLOCK)), OperationCancellation(),
            )
            log("usb: connected over ${channel.transport()}")
            val probe = channel.probe(16u, 20u, 3000u)
            log("usb: round trip ${probe.answered}/${probe.sent} answered, p50 ${probe.rttP50Us} us, p95 ${probe.rttP95Us} us")
            // Every size: a transfer of whole USB packets must not stall either way.
            val sweepStarted = System.nanoTime()
            for (size in 1..SWEEP) {
                val message = ByteArray(size) { if (it == 0) MARK else (it + size).toByte() }
                channel.send(message)
                val echo = try {
                    withTimeout(5_000) { echo(channel) }
                } catch (e: Exception) {
                    return log("usb: STALL at size $size: $e")
                }
                if (!echo.contentEquals(message)) return log("usb: WRONG echo at size $size")
            }
            val perEcho = (System.nanoTime() - sweepStarted) / 1e6 / SWEEP
            log("usb: sizes 1..$SWEEP echoed, ${"%.2f".format(perEcho)} ms per echo")
            val bulk = ByteArray(BULK_BYTES) { if (it == 0) MARK else it.toByte() }
            val started = System.nanoTime()
            val received: Long = coroutineScope {
                val reader = async {
                    var bytes = 0L
                    repeat(BULK_MESSAGES) { bytes += echo(channel).size }
                    bytes
                }
                repeat(BULK_MESSAGES) { channel.send(bulk) }
                withTimeout(60_000) { reader.await() }
            }
            val seconds = (System.nanoTime() - started) / 1e9
            val megabits = received * 8 / seconds / 1e6
            log("usb: bulk $received bytes each way in ${"%.2f".format(seconds)} s (${"%.1f".format(megabits)} Mbit/s each way)")
            channel.closeChannel()
            log("usb: done")
        } catch (e: Exception) {
            log("usb: failed: $e")
        } finally {
            link?.end()
        }
    }
}
