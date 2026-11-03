package com.yongshuai.ipaloader

import org.json.JSONObject
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.URI
import java.util.Base64
import kotlin.concurrent.thread

/**
 * Local proxy endpoint (轉發 v2「代打」): a minimal HTTP/1.1 server
 * bound to 127.0.0.1 only. The site POSTs one JSON request per Apple
 * call to /<token>/fetch; instead of piping bytes through (the v1
 * WISP design, whose browser-originated TLS fingerprint Apple
 * rejects at the GrandSlam "complete" step), the request is handed
 * to the Rust core ([ProxyNative]) whose reqwest/rustls client —
 * the same stack as this app's own login — executes it and returns
 * the response for re-wrapping as JSON.
 *
 * Wire protocol (fixed; the site implements the other half):
 *   request  {"url","method","headers":{k:v},"body_b64"?}
 *   response {"status","headers":{k:v},"body_b64"}
 *   upstream failure -> HTTP 502 {"error":…}; policy refusal ->
 *   HTTP 403 {"error":"host not allowed"}. Every response carries
 *   CORS `Access-Control-Allow-Origin: *` /
 *   `Access-Control-Allow-Headers: *`; OPTIONS answers 204.
 * Destination policy is the WISP policy (Apple hosts, port 443, no
 * IP literals, no private resolutions), shared with [WispServer].
 * Bodies are never logged; the Rust side logs method/host/status.
 */
class ProxyServer(
    private val port: Int,
    private val expectedToken: () -> String,
    private val filesDirPath: String,
    private val log: (String) -> Unit = {},
) {
    @Volatile
    private var running = false
    private var listener: ServerSocket? = null

    /** Binds synchronously, so a taken port throws to the caller. */
    fun start() {
        val server = ServerSocket(port, 50, InetAddress.getByName("127.0.0.1"))
        listener = server
        running = true
        log("本機代理已啟動（port $port）")
        thread(isDaemon = true, name = "proxy-accept") {
            while (running) {
                val socket = try {
                    server.accept()
                } catch (e: Exception) {
                    break // listener closed by stop()
                }
                thread(isDaemon = true, name = "proxy-conn") { handle(socket) }
            }
        }
    }

    fun stop() {
        running = false
        try {
            listener?.close()
        } catch (e: Exception) {
            // Already closed.
        }
        listener = null
        log("本機代理已停止")
    }

    private fun handle(socket: Socket) {
        try {
            socket.use { s ->
                s.soTimeout = SOCKET_TIMEOUT_MS
                val input = BufferedInputStream(s.getInputStream())
                val output = BufferedOutputStream(s.getOutputStream())
                val head = readHead(input) ?: return
                val lines = head.split("\r\n")
                val parts = lines.firstOrNull()?.split(" ") ?: return
                if (parts.size < 2) return
                val method = parts[0].uppercase()
                val path = parts[1].substringBefore('?')
                var contentLength = 0
                for (line in lines.drop(1)) {
                    val idx = line.indexOf(':')
                    if (idx <= 0) continue
                    if (line.substring(0, idx).trim()
                            .equals("Content-Length", ignoreCase = true)
                    ) {
                        contentLength =
                            line.substring(idx + 1).trim().toIntOrNull() ?: 0
                    }
                }
                val body = if (contentLength > 0) {
                    readExactly(input, contentLength)
                } else {
                    ByteArray(0)
                }
                when {
                    method == "OPTIONS" ->
                        respond(output, 204, "No Content", null)
                    method == "POST" && path == "/${expectedToken()}/fetch" ->
                        handleFetch(output, body)
                    else -> respondJson(
                        output, 404, JSONObject().put("error", "not found"),
                    )
                }
            }
        } catch (e: Exception) {
            // Client went away mid-request or timed out; the peer is
            // gone, so there is nobody to report the failure to.
            log("本機代理連線中斷：${e.javaClass.simpleName}")
        }
    }

    private fun handleFetch(output: BufferedOutputStream, rawBody: ByteArray) {
        val request = try {
            JSONObject(String(rawBody, Charsets.UTF_8))
        } catch (e: Exception) {
            respondJson(output, 400, JSONObject().put("error", "invalid json"))
            return
        }
        val url = request.optString("url")
        val method = request.optString("method", "GET").uppercase()
        if (url.isEmpty()) {
            respondJson(output, 400, JSONObject().put("error", "missing url"))
            return
        }
        if (!isProxyTargetAllowed(url)) {
            log("本機代理擋下請求（政策）：$url")
            respondJson(output, 403, JSONObject().put("error", "host not allowed"))
            return
        }
        val headersJson = request.optJSONObject("headers")?.toString() ?: "{}"
        val bodyB64 = request.optString("body_b64", "")
        val body = try {
            if (bodyB64.isEmpty()) {
                ByteArray(0)
            } else {
                Base64.getDecoder().decode(bodyB64)
            }
        } catch (e: Exception) {
            respondJson(output, 400, JSONObject().put("error", "invalid body_b64"))
            return
        }
        val resultText = try {
            ProxyNative.nativeProxyFetch(url, method, headersJson, body, filesDirPath)
        } catch (e: UnsatisfiedLinkError) {
            respondJson(output, 502, JSONObject().put("error", "rust core missing"))
            return
        }
        val result = try {
            JSONObject(resultText)
        } catch (e: Exception) {
            log("本機代理：Rust 回應不是 JSON")
            respondJson(output, 502, JSONObject().put("error", "bad core response"))
            return
        }
        // A core {"error":…} means the upstream could not be reached
        // (or refused at the TLS layer): surface it as a 502 with the
        // same body shape. Anything else is the fixed success shape
        // and goes back verbatim with HTTP 200.
        if (result.has("error")) {
            respondJson(output, 502, result)
        } else {
            respondJson(output, 200, result)
        }
    }

    /** The WISP destination policy, restated for an https URL:
     *  scheme https, port 443, no IP literals, Apple host whitelist,
     *  and the resolved addresses must all be public (rebind guard,
     *  via the same helper the WISP engine connects through). The
     *  Rust client re-resolves when it connects; both sides check,
     *  so a name that flips between the two lookups would need to
     *  flip within milliseconds and is still bounded by reqwest's
     *  own TLS verification of the host name. */
    private fun isProxyTargetAllowed(url: String): Boolean {
        val uri = try {
            URI(url)
        } catch (e: Exception) {
            return false
        }
        if (!"https".equals(uri.scheme, ignoreCase = true)) return false
        val host = uri.host?.lowercase() ?: return false
        val port = if (uri.port == -1) 443 else uri.port
        if (port != 443) return false
        if (isIpLiteral(host)) return false
        val policy = WispPolicy()
        if (policy.hostPatterns.none { it.matches(host) }) return false
        return resolveAllowedAddress(policy, host) != null
    }

    /** Reads the header block (up to CRLFCRLF), ASCII-decoded.
     *  Returns null on EOF before any byte or an oversized head. */
    private fun readHead(input: BufferedInputStream): String? {
        val bytes = ArrayList<Byte>(1024)
        var matched = 0
        while (bytes.size <= MAX_HEAD_BYTES) {
            val b = input.read()
            if (b < 0) return if (bytes.isEmpty()) null else finishHead(bytes)
            bytes.add(b.toByte())
            matched = when {
                (matched == 0 || matched == 2) && b == '\r'.code -> matched + 1
                (matched == 1 || matched == 3) && b == '\n'.code -> matched + 1
                b == '\r'.code -> 1
                else -> 0
            }
            if (matched == 4) return finishHead(bytes)
        }
        return null
    }

    private fun finishHead(bytes: ArrayList<Byte>): String {
        val arr = ByteArray(bytes.size) { bytes[it] }
        // latin-1 keeps a 1:1 byte->char mapping for header text.
        return String(arr, Charsets.ISO_8859_1)
    }

    private fun readExactly(input: BufferedInputStream, length: Int): ByteArray {
        val out = ByteArray(length)
        var read = 0
        while (read < length) {
            val n = input.read(out, read, length - read)
            if (n < 0) break
            read += n
        }
        return if (read == length) out else out.copyOf(read)
    }

    private fun respondJson(
        output: BufferedOutputStream,
        code: Int,
        json: JSONObject,
    ) {
        val reason = when (code) {
            200 -> "OK"
            400 -> "Bad Request"
            403 -> "Forbidden"
            404 -> "Not Found"
            502 -> "Bad Gateway"
            else -> "Status"
        }
        respond(output, code, reason, json.toString().toByteArray(Charsets.UTF_8))
    }

    /** One request per connection (Connection: close); the CORS
     *  headers go on every response, errors included, so the page
     *  can always read the outcome. */
    private fun respond(
        output: BufferedOutputStream,
        code: Int,
        reason: String,
        body: ByteArray?,
    ) {
        val head = buildString {
            append("HTTP/1.1 ").append(code).append(' ').append(reason).append("\r\n")
            append("Access-Control-Allow-Origin: *\r\n")
            append("Access-Control-Allow-Headers: *\r\n")
            append("Access-Control-Allow-Methods: POST, OPTIONS\r\n")
            if (body != null) append("Content-Type: application/json\r\n")
            append("Content-Length: ").append(body?.size ?: 0).append("\r\n")
            append("Connection: close\r\n\r\n")
        }
        output.write(head.toByteArray(Charsets.ISO_8859_1))
        if (body != null) output.write(body)
        output.flush()
    }

    companion object {
        private const val MAX_HEAD_BYTES = 64 * 1024
        private const val SOCKET_TIMEOUT_MS = 120_000
    }
}
