package dev.pinemic

import android.content.Context
import android.media.AudioDeviceInfo
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.os.Build
import android.os.SystemClock
import java.io.IOException
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.Inet4Address
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.ServerSocket
import java.net.Socket
import java.util.Collections
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

const val AUDIO_PORT = 54545
const val DISCOVERY_PORT = 54546

private const val SAMPLE_RATE = 48_000
private const val FRAME_SAMPLES = 960            // 20 ms of PCM per read (matches Android Opus encoder frame)
private const val BITRATE = 96_000               // 96 kbps high-quality mono Opus
private const val HELLO_TIMEOUT_MS = 3_000L
private const val HELLO = "PMIC_HELLO1"
private const val BYE = "PMIC_BYE1"

data class MicDeviceOption(
    val id: Int, // -1 means Auto (prefer bottom/primary built-in mic)
val label: String
)

data class AudioSourceOption(
    val source: Int,
    val label: String
)

val AUDIO_SOURCE_OPTIONS = listOf(
    AudioSourceOption(
        MediaRecorder.AudioSource.VOICE_RECOGNITION,
        "Voice Recognition (Clean audio without DSP)"
    ),
    AudioSourceOption(
        MediaRecorder.AudioSource.CAMCORDER,
        "Camcorder (Camera wideband microphone)"
    ),
    AudioSourceOption(
        MediaRecorder.AudioSource.UNPROCESSED,
        "Unprocessed (Raw signal without amplification)"
    ),
    AudioSourceOption(
        MediaRecorder.AudioSource.MIC,
        "Default Mic (Standard Android)"
    )
)

/** Lists available physical microphone inputs on the phone. */
fun listMicDevices(context: Context): List<MicDeviceOption> {
    val result = mutableListOf(
        MicDeviceOption(-1, "Auto (Primary bottom microphone)")
    )
    val am = context.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return result
    val inputs = am.getDevices(AudioManager.GET_DEVICES_INPUTS)
    for (dev in inputs) {
        val typeName = when (dev.type) {
            AudioDeviceInfo.TYPE_BUILTIN_MIC -> "Built-in microphone"
            AudioDeviceInfo.TYPE_WIRED_HEADSET -> "Wired headset"
            AudioDeviceInfo.TYPE_USB_DEVICE, AudioDeviceInfo.TYPE_USB_HEADSET -> "USB microphone"
            AudioDeviceInfo.TYPE_BLUETOOTH_SCO, AudioDeviceInfo.TYPE_BLE_HEADSET -> "Bluetooth microphone"
            else -> continue
        }
        val addr = if (Build.VERSION.SDK_INT >= 28) dev.address else ""
        val addrDesc = when {
            addr.equals("bottom", ignoreCase = true) -> "Bottom / Primary"
            addr.equals("top", ignoreCase = true) -> "Top / Earpiece"
            addr.equals("back", ignoreCase = true) -> "Back (Camera)"
            addr.equals("front", ignoreCase = true) -> "Front"
            addr.isNotBlank() -> addr
            else -> "ID ${dev.id}"
        }
        result += MicDeviceOption(dev.id, "$typeName ($addrDesc)")
    }
    return result
}

/** Finds the requested physical AudioDeviceInfo, or automatically picks the bottom primary mic. */
private fun resolvePreferredDevice(context: Context, preferredId: Int): AudioDeviceInfo? {
    val am = context.getSystemService(Context.AUDIO_SERVICE) as? AudioManager ?: return null
    val inputs = am.getDevices(AudioManager.GET_DEVICES_INPUTS)
    if (preferredId != -1) {
        inputs.firstOrNull { it.id == preferredId }?.let { return it }
    }
    // Auto mode: prefer built-in microphone with address "bottom" (primary mic at bottom of phone)
    val builtinMics = inputs.filter { it.type == AudioDeviceInfo.TYPE_BUILTIN_MIC }
    if (Build.VERSION.SDK_INT >= 28) {
        builtinMics.firstOrNull { it.address.equals("bottom", ignoreCase = true) }?.let { return it }
    }
    // Fallback to the first built-in mic
    return builtinMics.firstOrNull()
}

/** Site-local IPv4 addresses of this phone, for display ("type this into the Linux app"). */
fun localIpv4Addresses(): List<String> = try {
    Collections.list(NetworkInterface.getNetworkInterfaces())
    .filter { it.isUp && !it.isLoopback }
    .flatMap { Collections.list(it.inetAddresses) }
    .filterIsInstance<Inet4Address>()
    .filter { it.isSiteLocalAddress }
    .map { it.hostAddress ?: "" }
    .filter { it.isNotEmpty() }
} catch (e: Exception) {
    emptyList()
}

/**
 * Captures the microphone, encodes 48 kHz mono Opus and serves it over BOTH transports at once:
 *
 *  - UDP :54545  (Wi-Fi). The Linux app sends PMIC_HELLO1 once per second; we stream to its
 *                source address and stop ~3 s after the last HELLO (or on PMIC_BYE1).
 *  - TCP :54545  (USB via `adb forward`). Frames are length-prefixed: [len u16 BE][frame].
 *
 * Frame = [seq u16 BE][opus packet]. A UDP beacon "PHONEMIC1;<name>;<port>" is broadcast to
 * :54546 every second so the Linux app can list this phone.
 */
class Streamer(
    private val context: Context,
        private val onStatus: (String) -> Unit
) {

    companion object {
        @Volatile var selectedDeviceId: Int = -1
        @Volatile var selectedAudioSource: Int = MediaRecorder.AudioSource.VOICE_RECOGNITION
    }

    private val running = AtomicBoolean(false)
    @Volatile private var restartCapture = false
    private val workers = mutableListOf<Thread>()

    @Volatile private var udpSocket: DatagramSocket? = null
    @Volatile private var udpPeer: InetSocketAddress? = null
    @Volatile private var lastHello = 0L
    @Volatile private var tcpServer: ServerSocket? = null
    @Volatile private var tcpClient: Socket? = null
    private var seq = 0

    fun start() {
        if (!running.compareAndSet(false, true)) return
            onStatus("Waiting for the Linux app (port $AUDIO_PORT)…")
            workers += thread(name = "pm-udp") { udpLoop() }
            workers += thread(name = "pm-tcp") { tcpLoop() }
            workers += thread(name = "pm-beacon") { beaconLoop() }
            workers += thread(name = "pm-capture", priority = Thread.MAX_PRIORITY) { captureLoop() }
    }

    /** Re-initializes AudioRecord on the fly when the user selects another mic or source. */
    fun requestCaptureRestart() {
        restartCapture = true
    }

    fun stop() {
        if (!running.compareAndSet(true, false)) return
            // Politely tell a Wi-Fi peer we are gone, then close everything to unblock the threads.
            udpPeer?.let { peer ->
                try { udpSocket?.send(DatagramPacket(BYE.toByteArray(), BYE.length, peer)) } catch (_: Exception) {}
            }
            try { udpSocket?.close() } catch (_: Exception) {}
            try { tcpServer?.close() } catch (_: Exception) {}
            try { tcpClient?.close() } catch (_: Exception) {}
            workers.forEach { it.interrupt() }
            workers.forEach { try { it.join(1_000) } catch (_: InterruptedException) {} }
            workers.clear()
            udpPeer = null
            tcpClient = null
    }

    // ---------------------------------------------------------------- UDP (Wi-Fi)

    private fun udpLoop() {
        val sock = try {
            DatagramSocket(null).apply {
                reuseAddress = true
                bind(InetSocketAddress(AUDIO_PORT))
            }
        } catch (e: Exception) {
            onStatus("UDP port $AUDIO_PORT unavailable: ${e.message}")
            return
        }
        udpSocket = sock
        val buf = ByteArray(64)
        val pkt = DatagramPacket(buf, buf.size)
        while (running.get()) {
            pkt.length = buf.size
            try {
                sock.receive(pkt)
            } catch (e: IOException) {
                if (!running.get()) break else continue
            }
            val msg = String(buf, 0, pkt.length, Charsets.US_ASCII)
            when {
                msg.startsWith(HELLO) -> {
                    val peer = InetSocketAddress(pkt.address, pkt.port)
                    lastHello = SystemClock.elapsedRealtime()
                    if (udpPeer != peer) {
                        udpPeer = peer
                        onStatus("Streaming over Wi-Fi to ${peer.address.hostAddress}")
                    }
                }
                msg.startsWith(BYE) -> {
                    udpPeer = null
                    onStatus("Linux app disconnected. Waiting…")
                }
            }
        }
    }

    // ---------------------------------------------------------------- TCP (USB / adb forward)

    private fun tcpLoop() {
        val server = try {
            ServerSocket().apply {
                reuseAddress = true
                bind(InetSocketAddress(AUDIO_PORT))
            }
        } catch (e: Exception) {
            onStatus("TCP port $AUDIO_PORT unavailable: ${e.message}")
            return
        }
        tcpServer = server
        while (running.get()) {
            val client = try {
                server.accept()
            } catch (e: IOException) {
                break
            }
            try { client.tcpNoDelay = true } catch (_: Exception) {}
            try { tcpClient?.close() } catch (_: Exception) {}
            tcpClient = client
            onStatus("Streaming over USB (adb)")
        }
    }

    // ---------------------------------------------------------------- discovery beacon

    private fun broadcastAddresses(): List<InetAddress> {
        val list = mutableListOf<InetAddress>()
        try { list += InetAddress.getByName("255.255.255.255") } catch (_: Exception) {}
        try {
            for (ni in Collections.list(NetworkInterface.getNetworkInterfaces())) {
                if (!ni.isUp || ni.isLoopback) continue
                    for (ia in ni.interfaceAddresses) ia.broadcast?.let { list += it }
            }
        } catch (_: Exception) {}
        return list.distinct()
    }

    private fun beaconLoop() {
        val sock = try {
            DatagramSocket().apply { broadcast = true }
        } catch (e: Exception) {
            return
        }
        val name = Build.MODEL.replace(';', ' ')
        val payload = "PHONEMIC1;$name;$AUDIO_PORT".toByteArray(Charsets.UTF_8)
        try {
            while (running.get()) {
                for (addr in broadcastAddresses()) {
                    try { sock.send(DatagramPacket(payload, payload.size, addr, DISCOVERY_PORT)) } catch (_: IOException) {}
                }
                try { Thread.sleep(1_000) } catch (_: InterruptedException) { break }
            }
        } finally {
            sock.close()
        }
    }

    // ---------------------------------------------------------------- capture + encode

    private fun openAudioRecord(source: Int, minBuf: Int): AudioRecord? {
        val bufSize = maxOf(minBuf, FRAME_SAMPLES * 2 * 4)
        val candidates = listOf(source, MediaRecorder.AudioSource.VOICE_RECOGNITION, MediaRecorder.AudioSource.MIC).distinct()
        for (src in candidates) {
            try {
                val rec = AudioRecord(
                    src,
                    SAMPLE_RATE,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT,
                    bufSize
                )
                if (rec.state == AudioRecord.STATE_INITIALIZED) {
                    return rec
                }
                rec.release()
            } catch (_: Exception) {}
        }
        return null
    }

    private fun captureLoop() {
        val minBuf = AudioRecord.getMinBufferSize(
            SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
        )

        while (running.get()) {
            restartCapture = false
            var record: AudioRecord? = null
            var codec: MediaCodec? = null
            var ns: NoiseSuppressor? = null
            var aec: AcousticEchoCanceler? = null
            var agc: AutomaticGainControl? = null

            try {
                record = openAudioRecord(selectedAudioSource, minBuf)
                if (record == null) {
                    onStatus("ERROR: microphone could not be opened")
                    return
                }

                // Force-disable Android's built-in voice call DSP effects so the sound isn't muffled.
                val sessionId = record.audioSessionId
                try {
                    if (NoiseSuppressor.isAvailable()) {
                        ns = NoiseSuppressor.create(sessionId)?.apply { enabled = false }
                    }
                } catch (_: Exception) {}
                try {
                    if (AcousticEchoCanceler.isAvailable()) {
                        aec = AcousticEchoCanceler.create(sessionId)?.apply { enabled = false }
                    }
                } catch (_: Exception) {}
                try {
                    if (AutomaticGainControl.isAvailable()) {
                        agc = AutomaticGainControl.create(sessionId)?.apply { enabled = false }
                    }
                } catch (_: Exception) {}

                // Route to the primary bottom microphone (or user-selected mic).
                resolvePreferredDevice(context, selectedDeviceId)?.let { dev ->
                    try { record.setPreferredDevice(dev) } catch (_: Exception) {}
                }

                val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, 1).apply {
                    setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
                    setInteger(MediaFormat.KEY_COMPLEXITY, 10)
                    setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, FRAME_SAMPLES * 4)
                }
                codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).also {
                    it.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                    it.start()
                }
                record.startRecording()

                // Re-apply preferred device after startRecording() for OEM HALs that reset routing on start.
                resolvePreferredDevice(context, selectedDeviceId)?.let { dev ->
                    try { record.setPreferredDevice(dev) } catch (_: Exception) {}
                }

                val pcm = ShortArray(FRAME_SAMPLES)
                val bytes = ByteArray(FRAME_SAMPLES * 2)
                val info = MediaCodec.BufferInfo()
                var ptsUs = 0L

                while (running.get() && !restartCapture) {
                    // Blocking read: paces the loop at one 20 ms chunk (960 samples).
                    var got = 0
                    while (got < FRAME_SAMPLES && running.get() && !restartCapture) {
                        val n = record.read(pcm, got, FRAME_SAMPLES - got)
                        if (n < 0) {
                            onStatus("ERROR: microphone read failed ($n)")
                            return
                        }
                        got += n
                    }
                    if (got < FRAME_SAMPLES || restartCapture) break

                        // Short[] -> little-endian bytes.
                        for (i in 0 until FRAME_SAMPLES) {
                            val sample = pcm[i].toInt()
                            bytes[i * 2] = (sample and 0xFF).toByte()
                            bytes[i * 2 + 1] = ((sample shr 8) and 0xFF).toByte()
                        }

                        val inIdx = codec.dequeueInputBuffer(10_000)
                        if (inIdx >= 0) {
                            val inBuf = codec.getInputBuffer(inIdx)!!
                            inBuf.clear()
                            inBuf.put(bytes)
                            codec.queueInputBuffer(inIdx, 0, bytes.size, ptsUs, 0)
                            ptsUs += 20_000L
                        }

                        // Drain whatever the encoder has ready.
                        while (true) {
                            val outIdx = codec.dequeueOutputBuffer(info, 0)
                            if (outIdx >= 0) {
                                if (info.size > 0 && (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG) == 0) {
                                    val out = codec.getOutputBuffer(outIdx)!!
                                    out.position(info.offset)
                                    out.limit(info.offset + info.size)
                                    val opus = ByteArray(info.size)
                                    out.get(opus)
                                    sendOpus(opus)
                                }
                                codec.releaseOutputBuffer(outIdx, false)
                            } else if (outIdx == MediaCodec.INFO_TRY_AGAIN_LATER) {
                                break
                            }
                        }
                }
            } catch (e: SecurityException) {
                onStatus("ERROR: microphone permission denied")
                return
            } catch (e: Exception) {
                onStatus("ERROR: ${e.javaClass.simpleName}: ${e.message}")
                return
            } finally {
                try { ns?.release() } catch (_: Exception) {}
                try { aec?.release() } catch (_: Exception) {}
                try { agc?.release() } catch (_: Exception) {}
                try { record?.stop() } catch (_: Exception) {}
                try { record?.release() } catch (_: Exception) {}
                try { codec?.stop() } catch (_: Exception) {}
                try { codec?.release() } catch (_: Exception) {}
            }
        }
    }

    private fun sendOpus(opus: ByteArray) {
        val s = seq and 0xFFFF
        seq++
        val frame = ByteArray(2 + opus.size)
        frame[0] = (s shr 8).toByte()
        frame[1] = s.toByte()
        System.arraycopy(opus, 0, frame, 2, opus.size)

        // Wi-Fi / UDP: only while HELLOs keep arriving.
        val peer = udpPeer
        val sock = udpSocket
        if (peer != null && sock != null) {
            if (SystemClock.elapsedRealtime() - lastHello > HELLO_TIMEOUT_MS) {
                udpPeer = null
                onStatus("Linux app stopped responding. Waiting…")
            } else {
                try { sock.send(DatagramPacket(frame, frame.size, peer)) } catch (_: IOException) {}
            }
        }

        // USB / TCP: [len u16 BE][frame] in a single write.
        val client = tcpClient
        if (client != null) {
            try {
                val msg = ByteArray(2 + frame.size)
                msg[0] = (frame.size shr 8).toByte()
                msg[1] = frame.size.toByte()
                System.arraycopy(frame, 0, msg, 2, frame.size)
                val out = client.getOutputStream()
                out.write(msg)
                out.flush()
            } catch (e: IOException) {
                try { client.close() } catch (_: Exception) {}
                if (tcpClient === client) tcpClient = null
                    onStatus("USB client disconnected. Waiting…")
            }
        }
    }
}
