package com.yongshuai.ipaloader

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.io.File

/**
 * 0.6.2: the pairing tab. This is the pairing-file import block
 * that used to head both the sign and install pages (M3.1),
 * presented once as its own menu entry so the flow is not
 * duplicated across pages. The implementation is unchanged: the
 * picked lockdown pairing file is parsed and stored by the Rust
 * core (through the sign panel's JNI facade, whose class the
 * core's symbols are bound to), and every flow keeps reading the
 * same stored pairing.
 */
class PairingPanel(
    private val host: Activity,
    private val signFacade: SignActivity
) {

    private val filesDir: File get() = host.filesDir
    private val contentResolver get() = host.contentResolver
    private fun getString(resId: Int, vararg formatArgs: Any): String =
        host.getString(resId, *formatArgs)

    private fun runOnUiThread(action: () -> Unit) {
        host.runOnUiThread(action)
    }

    private fun startActivityForResult(intent: Intent, requestCode: Int) {
        host.startActivityForResult(intent, requestCode)
    }

    private val deviceText: TextView
    private val importButton: Button

    /** The panel's view hierarchy, built once and kept by the host. */
    val root: View

    init {
        val layout = LinearLayout(host).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }
        val deviceTitle = TextView(host).apply {
            textSize = 16f
            text = getString(R.string.sign_device_title)
        }
        deviceText = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.sign_device_none)
        }
        importButton = Button(host).apply {
            text = getString(R.string.sign_import_pairing)
            setOnClickListener { pickPairing() }
        }
        layout.addView(deviceTitle)
        layout.addView(deviceText)
        layout.addView(importButton)
        root = layout
    }

    private var opened = false

    /** First-display hook, mirroring the old page-open behavior. */
    fun onFirstShown() {
        if (opened) return
        opened = true
        AppLog.write(filesDir, "開啟配對頁")
        refreshPairingUi()
    }

    private fun refreshPairingUi() {
        Thread {
            val result = try {
                signFacade.nativeGetPairing(filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "NONE"
            }
            runOnUiThread {
                val parts = result.split("\n", limit = 3)
                if (parts[0] == "OK") {
                    showPairing(parts.getOrElse(1) { "" }, parts.getOrElse(2) { "" })
                } else {
                    deviceText.text = getString(R.string.sign_device_none)
                }
            }
        }.start()
    }

    /** Shows the stored target device: name (if any) + UDID tail. */
    private fun showPairing(udid: String, name: String) {
        val tail = udid.takeLast(8)
        deviceText.text = if (name.isNotEmpty()) {
            getString(R.string.sign_device_info, name, tail)
        } else {
            getString(R.string.sign_device_info_noname, tail)
        }
    }

    private fun pickPairing() {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
        }
        startActivityForResult(intent, REQ_PICK_PAIRING)
    }

    /** Picker results, forwarded by the host activity. */
    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (resultCode != Activity.RESULT_OK) return
        val uri = data?.data ?: return
        when (requestCode) {
            REQ_PICK_PAIRING -> onPairingPicked(uri)
        }
    }

    private fun onPairingPicked(uri: Uri) {
        deviceText.text = getString(R.string.sign_importing)
        Thread {
            val tmp = copyPickedPairing(uri)
            val result = if (tmp == null) {
                "ERR\nPAIRING\n無法讀取所選檔案"
            } else {
                try {
                    signFacade.nativeImportPairing(tmp.absolutePath, filesDir.absolutePath)
                } catch (e: UnsatisfiedLinkError) {
                    "ERR\nUNKNOWN\nRust core missing"
                }
            }
            runOnUiThread { onPairingResult(result) }
        }.start()
    }

    private fun copyPickedPairing(uri: Uri): File? {
        val out = File(filesDir, "pairing_import.plist")
        return try {
            contentResolver.openInputStream(uri)?.use { input ->
                out.outputStream().use { output -> input.copyTo(output) }
            } ?: return null
            out
        } catch (e: Exception) {
            null
        }
    }

    private fun onPairingResult(result: String) {
        val parts = result.split("\n", limit = 3)
        when (parts[0]) {
            "OK" -> {
                val udid = parts.getOrElse(1) { "" }
                val name = parts.getOrElse(2) { "" }
                showPairing(udid, name)
                Toast.makeText(host, getString(R.string.sign_import_ok), Toast.LENGTH_SHORT)
                    .show()
                AppLog.write(filesDir, "配對：已導入配對檔（裝置 …${udid.takeLast(6)}）")
            }
            else -> {
                Toast.makeText(
                    host,
                    getString(R.string.sign_import_failed, parts.getOrElse(2) { "" }),
                    Toast.LENGTH_LONG
                ).show()
                AppLog.write(filesDir, "配對：導入配對檔失敗（${parts.getOrElse(1) { "?" }}）")
                refreshPairingUi()
            }
        }
    }

    companion object {
        private const val REQ_PICK_PAIRING = 4301
    }
}
