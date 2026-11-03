package com.yongshuai.ipaloader

/**
 * JNI bridge for the local proxy (轉發 v2「代打」): the Rust core
 * executes one HTTP request through a reqwest client built exactly
 * like isideload's own GrandSlam client, so the TLS fingerprint
 * Apple sees is the app login's own rustls stack, not the browser's
 * libcurl. Used by [ProxyServer]; kept off [MainActivity] because
 * the caller is the relay service, not the UI.
 */
object ProxyNative {

    init {
        // MainActivity's companion loads the core too; loading here
        // as well covers a process that was restarted straight into
        // the foreground service. A second loadLibrary is a no-op.
        try {
            System.loadLibrary("ipaloader_core")
        } catch (e: UnsatisfiedLinkError) {
            // Already loaded.
        }
    }

    /**
     * Executes the request and returns a JSON string:
     * `{"status":…,"headers":{…},"body_b64":…}` on success or
     * `{"error":…}` when the upstream could not be reached.
     * [headersJson] is a flat JSON object of header name → value;
     * [body] is the raw request body (empty when none). Blocking —
     * call from a worker thread. Request/response bodies are never
     * logged by either side of this bridge.
     */
    external fun nativeProxyFetch(
        url: String,
        method: String,
        headersJson: String,
        body: ByteArray,
        filesDir: String,
    ): String
}
