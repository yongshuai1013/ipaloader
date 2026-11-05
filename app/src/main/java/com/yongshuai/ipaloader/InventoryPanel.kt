package com.yongshuai.ipaloader

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
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
 *
 * 0.6.4: a signing-key section below the App ID list. The portal
 * certificate's private key never leaves the device that created
 * it, so a second Android device cannot reuse the certificate
 * (and Apple answers a fresh CSR with 7460). Export writes the
 * stored key to a user-picked file through the core
 * (`MainActivity.nativeExportSigningKey`); import stores a picked
 * export file's key through `nativeImportSigningKey`, after an
 * explicit confirmation. The key itself is never displayed —
 * only the account and a public-key fingerprint.
 */
class InventoryPanel(
    private val host: MainActivity
) {

    private val filesDir: File get() = host.filesDir
    private val contentResolver get() = host.contentResolver
    private fun getString(resId: Int, vararg formatArgs: Any): String =
        host.getString(resId, *formatArgs)

    private fun startActivityForResult(intent: Intent, requestCode: Int) {
        host.startActivityForResult(intent, requestCode)
    }

    private fun runOnUiThread(action: () -> Unit) {
        host.runOnUiThread(action)
    }

    private val refreshButton: Button
    private val statusText: TextView
    private val certText: TextView
    private val appIdText: TextView
    private val keyStatus: TextView
    private val exportKeyButton: Button
    private val importKeyButton: Button

    private var busy = false
    private var opened = false
    private var keyBusy = false
    private var pendingExportFile: File? = null
    private var pendingExportEmail = ""
    private var pendingExportFingerprint = ""

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
        val keyTitle = TextView(host).apply {
            textSize = 16f
            setPadding(0, 32, 0, 8)
            text = getString(R.string.key_title)
        }
        val keyHint = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.key_hint)
        }
        keyStatus = TextView(host).apply { textSize = 15f }
        exportKeyButton = Button(host).apply {
            text = getString(R.string.key_export)
            setOnClickListener { startExportKey() }
        }
        importKeyButton = Button(host).apply {
            text = getString(R.string.key_import)
            setOnClickListener { startImportKey() }
        }
        layout.addView(keyTitle)
        layout.addView(keyHint)
        layout.addView(keyStatus)
        layout.addView(exportKeyButton)
        layout.addView(importKeyButton)
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

    // -- 0.6.4: signing-key export / import -------------------------

    private fun setKeyBusy(value: Boolean) {
        keyBusy = value
        exportKeyButton.isEnabled = !value
        importKeyButton.isEnabled = !value
    }

    /** Picker results, forwarded by the host activity. */
    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        when (requestCode) {
            REQ_EXPORT_KEY -> {
                if (resultCode != Activity.RESULT_OK) {
                    pendingExportFile = null
                    setKeyBusy(false)
                    return
                }
                val uri = data?.data ?: run {
                    pendingExportFile = null
                    setKeyBusy(false)
                    return
                }
                onExportTargetChosen(uri)
            }
            REQ_IMPORT_KEY -> {
                if (resultCode != Activity.RESULT_OK) return
                val uri = data?.data ?: return
                onImportKeyPicked(uri)
            }
        }
    }

    private fun startExportKey() {
        if (keyBusy) return
        setKeyBusy(true)
        keyStatus.text = getString(R.string.key_working)
        Thread {
            val dir = File(filesDir, "export").apply { mkdirs() }
            val tmp = File(dir, "ipaloader-signing-key.plist")
            val result = try {
                host.nativeExportSigningKey(filesDir.absolutePath, tmp.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onExportKeyReady(result, tmp) }
        }.start()
    }

    private fun onExportKeyReady(result: String, tmp: File) {
        val parts = result.split("\n", limit = 3)
        if (parts[0] == "OK") {
            pendingExportFile = tmp
            pendingExportEmail = parts.getOrElse(1) { "" }
            pendingExportFingerprint = parts.getOrElse(2) { "" }
            keyStatus.text = getString(R.string.key_working)
            val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "*/*"
                putExtra(Intent.EXTRA_TITLE, "ipaloader-signing-key.plist")
            }
            startActivityForResult(intent, REQ_EXPORT_KEY)
        } else {
            setKeyBusy(false)
            val code = parts.getOrElse(1) { "UNKNOWN" }
            val message = parts.getOrElse(2) { "" }
            keyStatus.text = when (code) {
                "NO_SESSION" -> getString(R.string.sign_no_session)
                "PANIC" -> getString(R.string.error_panic)
                else -> message
            }
            AppLog.write(filesDir, "簽名私鑰：導出失敗（$code）")
        }
    }

    private fun onExportTargetChosen(uri: Uri) {
        val tmp = pendingExportFile
        val email = pendingExportEmail
        val fingerprint = pendingExportFingerprint
        pendingExportFile = null
        Thread {
            val ok = tmp != null && try {
                contentResolver.openOutputStream(uri)?.use { output ->
                    tmp.inputStream().use { input -> input.copyTo(output) }
                } != null
            } catch (e: Exception) {
                false
            }
            runOnUiThread {
                setKeyBusy(false)
                if (ok) {
                    keyStatus.text = getString(R.string.key_export_done, email, fingerprint)
                    AppLog.write(filesDir, "簽名私鑰：已導出（帳號 $email）")
                } else {
                    keyStatus.text = getString(R.string.sign_copy_failed)
                    AppLog.write(filesDir, "簽名私鑰：導出寫入所選位置失敗")
                }
            }
        }.start()
    }

    private fun startImportKey() {
        if (keyBusy) return
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
        }
        startActivityForResult(intent, REQ_IMPORT_KEY)
    }

    private fun onImportKeyPicked(uri: Uri) {
        setKeyBusy(true)
        keyStatus.text = getString(R.string.key_working)
        Thread {
            val tmp = copyPickedKey(uri)
            runOnUiThread {
                if (tmp == null) {
                    setKeyBusy(false)
                    keyStatus.text = getString(R.string.sign_copy_failed)
                    AppLog.write(filesDir, "簽名私鑰：複製所選檔案失敗")
                } else {
                    confirmImportKey(tmp)
                }
            }
        }.start()
    }

    private fun copyPickedKey(uri: Uri): File? {
        val dir = File(filesDir, "tmp").apply { mkdirs() }
        val out = File(dir, "import-key.plist")
        return try {
            contentResolver.openInputStream(uri)?.use { input ->
                out.outputStream().use { output -> input.copyTo(output) }
            } ?: return null
            out
        } catch (e: Exception) {
            null
        }
    }

    private fun confirmImportKey(tmp: File) {
        AlertDialog.Builder(host)
            .setTitle(R.string.key_import_confirm_title)
            .setMessage(R.string.key_import_confirm_body)
            .setPositiveButton(R.string.key_import) { _, _ -> doImportKey(tmp) }
            .setNegativeButton(R.string.cancel) { _, _ -> setKeyBusy(false) }
            .setOnCancelListener { setKeyBusy(false) }
            .show()
    }

    private fun doImportKey(tmp: File) {
        keyStatus.text = getString(R.string.key_working)
        Thread {
            val result = try {
                host.nativeImportSigningKey(tmp.absolutePath, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onImportKeyResult(result) }
        }.start()
    }

    private fun onImportKeyResult(result: String) {
        setKeyBusy(false)
        val parts = result.split("\n", limit = 3)
        if (parts[0] == "OK") {
            val email = parts.getOrElse(1) { "" }
            val fingerprint = parts.getOrElse(2) { "" }
            keyStatus.text = getString(R.string.key_import_done, email, fingerprint)
            AppLog.write(filesDir, "簽名私鑰：已導入（帳號 $email）")
        } else {
            val code = parts.getOrElse(1) { "UNKNOWN" }
            val message = parts.getOrElse(2) { "" }
            keyStatus.text = if (code == "PANIC") {
                getString(R.string.error_panic)
            } else {
                message
            }
            AppLog.write(filesDir, "簽名私鑰：導入失敗（$code）")
        }
    }

    companion object {
        private const val REQ_EXPORT_KEY = 4501
        private const val REQ_IMPORT_KEY = 4502
    }
}
