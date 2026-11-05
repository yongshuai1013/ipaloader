package com.yongshuai.ipaloader

import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONArray
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/**
 * 0.6.2: the certificates / App IDs tab. Read-only view of the
 * account's portal inventory: the core restores the saved session
 * and runs the same list calls the sign flow uses
 * (`MainActivity.nativeListInventory`), and this panel only
 * renders the result. Nothing here creates, revokes or deletes.
 * Querying needs a session by nature; the tab itself is always
 * reachable, and a missing session is reported in place.
 */
class InventoryPanel(
    private val host: MainActivity
) {

    private val filesDir: File get() = host.filesDir
    private fun getString(resId: Int, vararg formatArgs: Any): String =
        host.getString(resId, *formatArgs)

    private fun runOnUiThread(action: () -> Unit) {
        host.runOnUiThread(action)
    }

    private val refreshButton: Button
    private val statusText: TextView
    private val certText: TextView
    private val appIdText: TextView

    private var busy = false
    private var opened = false

    /** The panel's view hierarchy, built once and kept by the host. */
    val root: View

    init {
        val layout = LinearLayout(host).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }
        refreshButton = Button(host).apply {
            text = getString(R.string.inv_refresh)
            setOnClickListener { startQuery() }
        }
        statusText = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.inv_hint)
        }
        val certTitle = TextView(host).apply {
            textSize = 16f
            setPadding(0, 32, 0, 8)
            text = getString(R.string.inv_certs_title)
        }
        certText = TextView(host).apply { textSize = 15f }
        val appIdTitle = TextView(host).apply {
            textSize = 16f
            setPadding(0, 32, 0, 8)
            text = getString(R.string.inv_appids_title)
        }
        appIdText = TextView(host).apply { textSize = 15f }
        layout.addView(refreshButton)
        layout.addView(statusText)
        layout.addView(certTitle)
        layout.addView(certText)
        layout.addView(appIdTitle)
        layout.addView(appIdText)
        root = layout
    }

    /** First-display hook: open with a fresh query, like the old pages. */
    fun onFirstShown() {
        if (opened) return
        opened = true
        AppLog.write(filesDir, "開啟證書・App ID 頁")
        startQuery()
    }

    private fun startQuery() {
        if (busy) return
        busy = true
        refreshButton.isEnabled = false
        statusText.text = getString(R.string.inv_loading)
        Thread {
            val result = try {
                host.nativeListInventory(filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onQueryResult(result) }
        }.start()
    }

    private fun onQueryResult(result: String) {
        busy = false
        refreshButton.isEnabled = true
        val head = result.substringBefore("\n")
        if (head == "OK") {
            val parts = result.split("\n", limit = 4)
            val team = parts.getOrElse(1) { "" }
            statusText.text = if (team.isNotEmpty()) {
                getString(R.string.team_label, team)
            } else {
                getString(R.string.team_unknown)
            }
            certText.text = renderCerts(parts.getOrElse(2) { "[]" })
            appIdText.text = renderAppIds(parts.getOrElse(3) { "[]" })
            AppLog.write(filesDir, "證書・App ID：查詢完成")
        } else {
            val parts = result.split("\n", limit = 3)
            val code = parts.getOrElse(1) { "UNKNOWN" }
            val message = parts.getOrElse(2) { "" }
            statusText.text = when (code) {
                "NO_SESSION" -> getString(R.string.sign_no_session)
                "PANIC" -> getString(R.string.error_panic)
                "NETWORK" -> getString(R.string.error_network)
                "ANISETTE" -> getString(R.string.error_anisette, message)
                "PORTAL" -> getString(R.string.sign_failed_portal, message)
                else -> getString(R.string.inv_failed, message)
            }
            AppLog.write(filesDir, "證書・App ID：查詢失敗（$code）")
        }
    }

    private fun renderCerts(json: String): String {
        return try {
            val array = JSONArray(json)
            if (array.length() == 0) return getString(R.string.inv_empty_certs)
            val lines = StringBuilder()
            for (i in 0 until array.length()) {
                val cert = array.getJSONObject(i)
                val expires = formatExpiry(cert.optString("expires")).substringBefore(" ")
                lines.append(
                    getString(
                        R.string.sign_cert_line,
                        cert.optString("name"),
                        cert.optString("machineName"),
                        expires
                    )
                )
                if (i < array.length() - 1) lines.append("\n")
            }
            lines.toString()
        } catch (e: Exception) {
            getString(R.string.inv_empty_certs)
        }
    }

    private fun renderAppIds(json: String): String {
        return try {
            val array = JSONArray(json)
            if (array.length() == 0) return getString(R.string.inv_empty_appids)
            val lines = StringBuilder()
            for (i in 0 until array.length()) {
                val appId = array.getJSONObject(i)
                val expires = formatExpiry(appId.optString("expires")).substringBefore(" ")
                lines.append(
                    getString(
                        R.string.inv_appid_line,
                        appId.optString("name"),
                        appId.optString("identifier"),
                        expires
                    )
                )
                if (i < array.length() - 1) lines.append("\n")
            }
            lines.toString()
        } catch (e: Exception) {
            getString(R.string.inv_empty_appids)
        }
    }

    /** RFC 3339 (UTC) from the core -> local "yyyy-MM-dd HH:mm". */
    private fun formatExpiry(iso: String): String {
        if (iso.isEmpty()) return getString(R.string.sign_expiry_unknown)
        return try {
            val parser = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss'Z'", Locale.US)
            parser.timeZone = TimeZone.getTimeZone("UTC")
            val date: Date = parser.parse(iso) ?: return iso
            SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.US).format(date)
        } catch (e: Exception) {
            iso
        }
    }
}
