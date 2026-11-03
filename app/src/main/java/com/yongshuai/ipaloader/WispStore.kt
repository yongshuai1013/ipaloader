package com.yongshuai.ipaloader

import android.content.Context
import java.net.Inet4Address
import java.net.NetworkInterface
import java.security.SecureRandom

/**
 * Persistent settings for the local WISP relay: the secret path
 * token (generated once, kept in app-private preferences) and the
 * URL shapes the UI shows. The relay port is fixed (see [PORT]) so
 * the URL stays stable across restarts and app updates.
 */
object WispStore {

    /** Fixed relay port: an unprivileged high port that does not
     *  collide with common service/dev ports (1080, 3128, 8000,
     *  8080, 8443, 8888, 9090 are all taken by something popular). */
    const val PORT = 18765

    /** Port of the v2 proxy endpoint ([ProxyServer]): the next
     *  number up from the WISP port. It binds 127.0.0.1 only — the
     *  proxy executes requests with the app's own credentials-grade
     *  network stack, so unlike the dumb WISP pipe it is never
     *  exposed to the LAN. */
    const val PROXY_PORT = 18766

    private const val PREFS = "wisp_relay"
    private const val KEY_SECRET = "secret"

    /** Returns the stored token, generating and saving one on first use. */
    fun secret(context: Context): String {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs.getString(KEY_SECRET, null)?.let { return it }
        return regenerate(context)
    }

    /** Generates a fresh token (32 hex chars) and stores it. Existing
     *  connections keep working; new ones must use the new path. */
    fun regenerate(context: Context): String {
        val bytes = ByteArray(16)
        SecureRandom().nextBytes(bytes)
        val token = bytes.joinToString("") { "%02x".format(it) }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit().putString(KEY_SECRET, token).apply()
        return token
    }

    /** The WISP endpoint path the server expects, "/<token>/". */
    fun expectedPath(context: Context): String = "/${secret(context)}/"

    fun loopbackUrl(context: Context): String =
        "ws://127.0.0.1:$PORT/${secret(context)}/"

    /** Base URL of the v2 proxy; the site appends "fetch" to it. */
    fun proxyUrl(context: Context): String =
        "http://127.0.0.1:$PROXY_PORT/${secret(context)}/"

    fun lanUrl(context: Context): String? =
        lanIpv4()?.let { "ws://$it:$PORT/${secret(context)}/" }

    /** Best-effort LAN IPv4: a site-local address (typical Wi-Fi)
     *  wins over any other non-loopback address (e.g. mobile data). */
    fun lanIpv4(): String? {
        var fallback: String? = null
        try {
            val interfaces = NetworkInterface.getNetworkInterfaces() ?: return null
            for (iface in interfaces) {
                if (!iface.isUp || iface.isLoopback || iface.isVirtual) continue
                for (addr in iface.inetAddresses) {
                    if (addr !is Inet4Address || addr.isLoopbackAddress) continue
                    val host = addr.hostAddress ?: continue
                    if (addr.isSiteLocalAddress) return host
                    if (fallback == null) fallback = host
                }
            }
        } catch (e: Exception) {
            // Enumeration is best-effort; the loopback URL always works.
        }
        return fallback
    }
}
