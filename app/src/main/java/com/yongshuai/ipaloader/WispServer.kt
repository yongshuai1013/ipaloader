package com.yongshuai.ipaloader

import org.java_websocket.WebSocket
import org.java_websocket.framing.CloseFrame
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.net.Inet4Address
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * WISP v1 relay engine ("本地轉發"): a WebSocket server that tunnels
 * TCP streams for the sideimpactor web login, so the site's traffic
 * leaves through this phone's own connection instead of a datacenter
 * egress (Apple's GrandSlam "complete" step rejects datacenter IPs;
 * see GOAL.md). Pure JVM on purpose (Java-WebSocket + java.net only)
 * so the framing can be exercised off-device; the Android lifecycle
 * lives in [WispRelayService].
 *
 * Protocol (byte-compatible with @mercuryworkshop/wisp-js v1, the
 * library the site's Worker backend and the wisp-render package run):
 * frame = [type u8][stream_id u32 LE][payload]; CONNECT payload =
 * [stream_type u8][port u16 LE][hostname UTF-8]; the server sends a
 * CONTINUE on stream 0 (buffer 128) as soon as a connection opens,
 * then pumps raw bytes both ways per stream. Destination policy
 * mirrors the site backend's wisp-policy.ts (see [WispPolicy]).
 */

/** Destination policy, mirroring the site's Worker wisp-policy. */
data class WispPolicy(
    val hostPatterns: List<Regex> = APPLE_HOST_PATTERNS,
    val allowedPorts: Set<Int> = setOf(443),
    val allowDirectIp: Boolean = false,
    val allowPrivateIps: Boolean = false,
) {
    companion object {
        val APPLE_HOST_PATTERNS: List<Regex> = listOf(
            Regex("""^auth\.itunes\.apple\.com$"""),
            Regex("""^buy\.itunes\.apple\.com$"""),
            Regex("""^init\.itunes\.apple\.com$"""),
            Regex("""^p\d+-buy\.itunes\.apple\.com$"""),
            Regex("""^gsa\.apple\.com$"""),
            Regex("""^gsd\.apple\.com$"""),
            Regex("""^developerservices2\.apple\.com$"""),
            // Hosts for the Apple Music libraries behind anisette.
            Regex("""^apps\.mzstatic\.com$"""),
        )
    }
}

class WispServer(
    port: Int,
    private val expectedPath: () -> String,
    private val policy: WispPolicy = WispPolicy(),
    private val log: (String) -> Unit = {},
    private val onStarted: () -> Unit = {},
    private val onFailed: (String) -> Unit = {},
) : WebSocketServer(InetSocketAddress("0.0.0.0", port)) {

    private val connections = ConcurrentHashMap<WebSocket, WispConnection>()

    @Volatile
    private var started = false

    init {
        isReuseAddr = true
        // Java-WebSocket's loss timer doubles as a keepalive ping to
        // the browser client (browsers answer WS pings automatically).
        connectionLostTimeout = 60
    }

    override fun onStart() {
        started = true
        log("WISP 伺服器已啟動（port ${address.port}）")
        onStarted()
    }

    override fun onOpen(conn: WebSocket, handshake: ClientHandshake) {
        val path = handshake.resourceDescriptor.substringBefore('?')
        if (path != expectedPath()) {
            // Not our endpoint: never speak WISP here. (wisp-render
            // answers 404 before the upgrade; closing the just-opened
            // socket is the equivalent for this embedded server.)
            log("WISP 拒絕未知路徑的連線")
            conn.close(CloseFrame.REFUSE, "not found")
            return
        }
        val connection = WispConnection(conn)
        connections[conn] = connection
        connection.sendInitialContinue()
    }

    override fun onClose(conn: WebSocket, code: Int, reason: String, remote: Boolean) {
        connections.remove(conn)?.closeAll()
    }

    override fun onMessage(conn: WebSocket, message: String) {
        // WISP packets only ever arrive as binary frames; ignore text.
    }

    override fun onMessage(conn: WebSocket, bytes: ByteBuffer) {
        val copy = ByteArray(bytes.remaining())
        bytes.get(copy)
        connections[conn]?.onPacket(copy)
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        if (conn == null) {
            log("WISP 伺服器錯誤：${ex.message}")
            if (!started) onFailed(ex.message ?: ex.javaClass.simpleName)
        } else {
            log("WISP 連線錯誤：${ex.message}")
        }
    }

    /** One WebSocket connection and the streams multiplexed over it. */
    private inner class WispConnection(private val ws: WebSocket) {
        private val sendLock = Any()
        private val streams = ConcurrentHashMap<Int, WispStream>()

        fun sendInitialContinue() {
            sendFrame(TYPE_CONTINUE, 0, u32le(INITIAL_BUFFER))
        }

        fun sendFrame(type: Int, streamId: Int, payload: ByteArray) {
            val frame = ByteArray(5 + payload.size)
            frame[0] = type.toByte()
            frame[1] = (streamId and 0xff).toByte()
            frame[2] = ((streamId ushr 8) and 0xff).toByte()
            frame[3] = ((streamId ushr 16) and 0xff).toByte()
            frame[4] = ((streamId ushr 24) and 0xff).toByte()
            payload.copyInto(frame, 5)
            synchronized(sendLock) {
                try {
                    ws.send(frame)
                } catch (e: Exception) {
                    // Peer already gone; streams notice on their own.
                }
            }
        }

        fun onPacket(bytes: ByteArray) {
            if (bytes.size < 5) return
            val type = bytes[0].toInt() and 0xff
            val streamId = (bytes[1].toInt() and 0xff) or
                ((bytes[2].toInt() and 0xff) shl 8) or
                ((bytes[3].toInt() and 0xff) shl 16) or
                ((bytes[4].toInt() and 0xff) shl 24)
            val payload = bytes.copyOfRange(5, bytes.size)
            when (type) {
                TYPE_CONNECT -> onConnect(streamId, payload)
                TYPE_DATA -> streams[streamId]?.enqueue(payload)
                TYPE_CLOSE -> {
                    val reason = if (payload.isNotEmpty()) {
                        payload[0].toInt() and 0xff
                    } else {
                        REASON_VOLUNTARY
                    }
                    streams.remove(streamId)?.closeFromClient(reason)
                }
                // A CONTINUE from a v1 client (and any unknown type)
                // is ignored, exactly like wisp-js (it only logs).
                else -> Unit
            }
        }

        private fun onConnect(streamId: Int, payload: ByteArray) {
            if (payload.size < 3) return
            val streamType = payload[0].toInt() and 0xff
            val port = (payload[1].toInt() and 0xff) or
                ((payload[2].toInt() and 0xff) shl 8)
            val hostname = String(payload, 3, payload.size - 3, Charsets.UTF_8).trim()
            streams.remove(streamId)?.closeQuietly()
            if (!isDestinationAllowed(policy, streamType, hostname, port)) {
                log("WISP 擋下連線：$hostname:$port（政策）")
                sendFrame(TYPE_CLOSE, streamId, byteArrayOf(REASON_HOST_BLOCKED.toByte()))
                return
            }
            log("WISP 開新 stream：$hostname:$port")
            val stream = WispStream(this, streamId, hostname, port)
            streams[streamId] = stream
            stream.start()
        }

        fun forget(streamId: Int, stream: WispStream) {
            streams.remove(streamId, stream)
        }

        fun closeAll() {
            for ((_, stream) in streams) stream.closeQuietly()
            streams.clear()
        }
    }

    /** One proxied TCP connection: client DATA queue -> socket, and
     *  socket bytes -> client DATA frames, each on its own thread. */
    private inner class WispStream(
        private val connection: WispConnection,
        private val streamId: Int,
        private val hostname: String,
        private val port: Int,
    ) {
        private val queue = LinkedBlockingQueue<ByteArray>()
        private val closed = AtomicBoolean(false)

        @Volatile
        private var socket: Socket? = null
        private var packetsWritten = 0

        fun enqueue(data: ByteArray) {
            if (!closed.get()) queue.offer(data)
        }

        fun start() {
            thread(isDaemon = true, name = "wisp-stream-$streamId") { connectAndRun() }
        }

        private fun connectAndRun() {
            val address = resolveAllowedAddress(policy, hostname)
            if (address == null) {
                log("WISP 擋下連線：$hostname（解析後位址不允許）")
                closeWithReason(REASON_HOST_BLOCKED)
                return
            }
            val sock = Socket()
            socket = sock
            try {
                sock.connect(InetSocketAddress(address, port), CONNECT_TIMEOUT_MS)
            } catch (e: Exception) {
                log("WISP 連線失敗：$hostname:$port（${e.javaClass.simpleName}）")
                closeWithReason(REASON_NETWORK_ERROR)
                return
            }
            if (closed.get()) {
                closeSocket()
                return
            }
            thread(isDaemon = true, name = "wisp-stream-$streamId-read") {
                pumpSocketToWs(sock)
            }
            pumpWsToSocket(sock)
        }

        private fun pumpWsToSocket(sock: Socket) {
            try {
                val out = sock.getOutputStream()
                while (!closed.get()) {
                    val data = queue.poll(200, TimeUnit.MILLISECONDS) ?: continue
                    out.write(data)
                    out.flush()
                    packetsWritten++
                    if (packetsWritten % REFILL_EVERY == 0) {
                        // Top up the client's per-stream send credit
                        // (v1 flow control counts DATA packets).
                        connection.sendFrame(TYPE_CONTINUE, streamId, u32le(INITIAL_BUFFER))
                    }
                }
            } catch (e: Exception) {
                // Socket closed under us or reset by the peer.
            } finally {
                closeWithReason(REASON_VOLUNTARY)
            }
        }

        private fun pumpSocketToWs(sock: Socket) {
            try {
                val input = sock.getInputStream()
                val buffer = ByteArray(READ_CHUNK)
                while (!closed.get()) {
                    val n = input.read(buffer)
                    if (n < 0) break
                    if (n == 0) continue
                    connection.sendFrame(TYPE_DATA, streamId, buffer.copyOf(n))
                }
            } catch (e: Exception) {
                // Socket closed under us or reset by the peer.
            } finally {
                closeWithReason(REASON_VOLUNTARY)
            }
        }

        /** Client sent CLOSE: tear down and echo CLOSE once (wisp-js
         *  answers a client CLOSE with a CLOSE of the same reason). */
        fun closeFromClient(reason: Int) {
            if (!closed.compareAndSet(false, true)) return
            closeSocket()
            connection.sendFrame(TYPE_CLOSE, streamId, byteArrayOf(reason.toByte()))
            connection.forget(streamId, this)
        }

        /** Connect failure, EOF, or a dead socket: notify the client
         *  once with [reason] and tear down. */
        fun closeWithReason(reason: Int) {
            if (!closed.compareAndSet(false, true)) return
            closeSocket()
            connection.sendFrame(TYPE_CLOSE, streamId, byteArrayOf(reason.toByte()))
            connection.forget(streamId, this)
        }

        /** The WebSocket itself is going away: no frame, just teardown. */
        fun closeQuietly() {
            if (!closed.compareAndSet(false, true)) return
            closeSocket()
            connection.forget(streamId, this)
        }

        private fun closeSocket() {
            try {
                socket?.close()
            } catch (e: Exception) {
                // Already closed.
            }
        }
    }

    companion object {
        private const val TYPE_CONNECT = 0x01
        private const val TYPE_DATA = 0x02
        private const val TYPE_CONTINUE = 0x03
        private const val TYPE_CLOSE = 0x04
        private const val INITIAL_BUFFER = 128
        private const val REFILL_EVERY = 32
        private const val READ_CHUNK = 16 * 1024
        private const val CONNECT_TIMEOUT_MS = 15_000

        // Close reasons (wisp-js close_reasons).
        private const val REASON_VOLUNTARY = 0x02
        private const val REASON_NETWORK_ERROR = 0x03
        private const val REASON_HOST_BLOCKED = 0x48
    }
}

private fun u32le(value: Int): ByteArray = byteArrayOf(
    (value and 0xff).toByte(),
    ((value ushr 8) and 0xff).toByte(),
    ((value ushr 16) and 0xff).toByte(),
    ((value ushr 24) and 0xff).toByte(),
)

/** Destination checks that need no DNS, in wisp-js filter order:
 *  TCP only, port whitelist, no IP literals, hostname whitelist. */
private fun isDestinationAllowed(
    policy: WispPolicy,
    streamType: Int,
    hostname: String,
    port: Int,
): Boolean {
    if (streamType != 0x01) return false
    if (port !in policy.allowedPorts) return false
    if (!policy.allowDirectIp && isIpLiteral(hostname)) return false
    return policy.hostPatterns.any { it.matches(hostname) }
}

private fun isIpLiteral(hostname: String): Boolean {
    if (hostname.contains(':')) return true // IPv6 literal
    return hostname.matches(Regex("""^\d{1,3}(\.\d{1,3}){3}$"""))
}

/**
 * Resolves [hostname] and returns the address to connect to, or null
 * when resolution fails or — while the policy forbids private
 * targets — any resolved address is non-public (DNS-rebind guard,
 * mirroring wisp-js is_ip_blocked after lookup). The socket connects
 * to the validated address itself, so check and use cannot drift.
 */
private fun resolveAllowedAddress(policy: WispPolicy, hostname: String): InetAddress? {
    val addresses = try {
        InetAddress.getAllByName(hostname)
    } catch (e: Exception) {
        return null
    }
    if (addresses.isEmpty()) return null
    if (!policy.allowPrivateIps && addresses.any { !isPublicAddress(it) }) return null
    return addresses.firstOrNull { it is Inet4Address } ?: addresses[0]
}

/** Public = not loopback/link-local/site-local/multicast/unspecified
 *  (java.net checks) and outside the extra non-routable IPv4 ranges
 *  ipaddr.js also treats as non-public (CGNAT, benchmarking,
 *  documentation prefixes, 240/4). */
private fun isPublicAddress(address: InetAddress): Boolean {
    if (address.isLoopbackAddress || address.isLinkLocalAddress ||
        address.isSiteLocalAddress || address.isMulticastAddress ||
        address.isAnyLocalAddress
    ) {
        return false
    }
    val bytes = address.address
    if (bytes.size == 4) {
        val b0 = bytes[0].toInt() and 0xff
        val b1 = bytes[1].toInt() and 0xff
        val b2 = bytes[2].toInt() and 0xff
        if (b0 == 0) return false // 0.0.0.0/8 "this network"
        if (b0 == 100 && b1 in 64..127) return false // CGNAT 100.64.0.0/10
        if (b0 == 192 && b1 == 0 && b2 == 0) return false // 192.0.0.0/24
        if (b0 == 192 && b1 == 0 && b2 == 2) return false // TEST-NET-1
        if (b0 == 198 && (b1 == 18 || b1 == 19)) return false // benchmarking /15
        if (b0 == 198 && b1 == 51 && b2 == 100) return false // TEST-NET-2
        if (b0 == 203 && b1 == 0 && b2 == 113) return false // TEST-NET-3
        if (b0 >= 240) return false // reserved + broadcast
    }
    return true
}
