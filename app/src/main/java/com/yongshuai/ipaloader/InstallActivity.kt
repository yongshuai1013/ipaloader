package com.yongshuai.ipaloader

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Uri
import android.provider.OpenableColumns
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.TextView
import java.io.File
import java.net.Inet4Address

/**
 * M4: install an already-signed IPA onto the iPhone over Wi-Fi.
 *
 * The target device is the one from the imported lockdown pairing
 * file (same store as the sign page). The Rust core finds the
 * phone on the LAN (subnet probe of the lockdownd port, then a
 * lockdown session verified against the pairing file's UDID),
 * reports its name / iOS version, and installs through isideload's
 * AFC + InstallationProxy flow with percent progress.
 *
 * Entries: the install tab itself (pick any signed IPA), the
 * sign tab's 「安裝到 iPhone」 button after a successful sign, and
 * the import tab — the latter two hand the IPA over pre-picked
 * through [prePick].
 *
 * 0.6.2: this class is no longer an Activity — it is the install
 * tab's panel, hosted by MainActivity. The class name is unchanged
 * because the Rust core's JNI symbols (and its progress callbacks,
 * which are delivered to the calling object) are bound to it.
 * Framework access goes through [host]. The pairing-file import
 * block that used to head this page now lives in the pairing tab
 * (PairingPanel); the probe below reads the stored pairing
 * through the core exactly as before.
 */
class InstallActivity(
    private val host: Activity
) {

    external fun nativeProbeDevice(phoneIp: String, prefixLen: Int, filesDir: String): String
    external fun nativeInstallIpa(
        ipaPath: String,
        phoneIp: String,
        prefixLen: Int,
        filesDir: String
    ): String

    // Framework delegates so the flow code below reads unchanged.
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

    private lateinit var connText: TextView
    private lateinit var probeButton: Button
    private lateinit var pickButton: Button
    private lateinit var fileText: TextView
    private lateinit var installButton: Button
    private lateinit var progressBar: ProgressBar
    private lateinit var percentText: TextView
    private lateinit var resultText: TextView

    private var pickedFile: File? = null
    private var busy = false

    /** The panel's view hierarchy, built once and kept by the host. */
    val root: View

    init {
        val layout = LinearLayout(host).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }

        val connTitle = TextView(host).apply {
            textSize = 16f
            setPadding(0, 32, 0, 0)
            text = getString(R.string.install_conn_title)
        }
        connText = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.install_conn_unknown)
        }
        probeButton = Button(host).apply {
            text = getString(R.string.install_probe)
            setOnClickListener { startProbe() }
        }

        pickButton = Button(host).apply {
            text = getString(R.string.install_pick)
            setOnClickListener { pickIpa() }
        }
        fileText = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.install_no_file)
        }
        installButton = Button(host).apply {
            text = getString(R.string.install_start)
            isEnabled = false
            setOnClickListener { startInstall() }
        }
        progressBar = ProgressBar(host, null, android.R.attr.progressBarStyleHorizontal)
            .apply {
                max = 100
                progress = 0
                visibility = View.GONE
                layoutParams = LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT,
                    LinearLayout.LayoutParams.WRAP_CONTENT
                )
            }
        percentText = TextView(host).apply {
            textSize = 15f
            visibility = View.GONE
        }
        resultText = TextView(host).apply {
            textSize = 15f
            setPadding(0, 32, 0, 0)
        }

        layout.addView(connTitle)
        layout.addView(connText)
        layout.addView(probeButton)
        layout.addView(pickButton)
        layout.addView(fileText)
        layout.addView(installButton)
        layout.addView(progressBar)
        layout.addView(percentText)
        layout.addView(resultText)

        root = layout
    }

    private var opened = false

    /** First-display hook, mirroring the old page-open behavior. */
    fun onFirstShown() {
        if (opened) return
        opened = true
        AppLog.write(filesDir, "開啟安裝頁")
        startProbe()
    }

    /**
     * IPA handed over pre-picked (the sign tab's fresh output, or
     * the import tab) — the file already lives in app storage.
     */
    fun prePick(file: File, displayName: String) {
        pickedFile = file
        fileText.text = getString(R.string.install_file_info, displayName)
        installButton.isEnabled = true
        AppLog.write(filesDir, "安裝：帶入 IPA（${file.name}）")
    }

    // -- Local subnet -----------------------------------------------------

    /** The phone's own Wi-Fi IPv4 address + prefix length, if any. */
    private fun localSubnet(): Pair<String, Int>? {
        try {
            val cm = host.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
            val network = cm.activeNetwork ?: return null
            val props = cm.getLinkProperties(network) ?: return null
            for (la in props.linkAddresses) {
                val addr = la.address
                if (addr is Inet4Address && !addr.isLoopbackAddress) {
                    val host = addr.hostAddress ?: continue
                    return host to la.prefixLength
                }
            }
        } catch (e: Exception) {
            // fall through: caller shows the "no Wi-Fi" message
        }
        return null
    }

    private fun setBusy(value: Boolean) {
        busy = value
        probeButton.isEnabled = !value
        pickButton.isEnabled = !value
        installButton.isEnabled = !value && pickedFile != null
    }

    // -- Connection probe ---------------------------------------------------

    private fun startProbe() {
        if (busy) return
        val subnet = localSubnet()
        if (subnet == null) {
            connText.text = getString(R.string.install_no_wifi)
            return
        }
        setBusy(true)
        connText.text = getString(R.string.install_probing)
        Thread {
            val result = try {
                nativeProbeDevice(subnet.first, subnet.second, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onProbeResult(result) }
        }.start()
    }

    private fun onProbeResult(result: String) {
        setBusy(false)
        val parts = result.split("\n", limit = 4)
        when (parts[0]) {
            "OK" -> {
                connText.text = getString(
                    R.string.install_conn_ok,
                    parts.getOrElse(2) { "" },
                    parts.getOrElse(3) { "" },
                    parts.getOrElse(1) { "" }
                )
                AppLog.write(
                    filesDir,
                    "安裝：裝置連線 OK（${parts.getOrElse(2) { "" }}，iOS ${parts.getOrElse(3) { "" }}，${parts.getOrElse(1) { "" }}）"
                )
            }
            else -> {
                val code = parts.getOrElse(1) { "UNKNOWN" }
                val message = parts.getOrElse(2) { "" }
                connText.text = when (code) {
                    "NOPAIRING" -> getString(R.string.install_device_none)
                    "NOTFOUND" -> getString(R.string.install_notfound)
                    "BADIP" -> getString(R.string.install_no_wifi)
                    "PANIC" -> getString(R.string.error_panic)
                    else -> message.ifEmpty { getString(R.string.install_conn_failed) }
                }
                AppLog.write(filesDir, "安裝：裝置連線失敗（$code）")
            }
        }
    }

    // -- Picking a signed IPA -------------------------------------------------

    private fun pickIpa() {
        if (busy) return
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
        }
        startActivityForResult(intent, REQ_PICK_IPA)
    }

    /** Picker results, forwarded by the host activity. */
    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (resultCode != Activity.RESULT_OK) return
        val uri = data?.data ?: return
        when (requestCode) {
            REQ_PICK_IPA -> onIpaPicked(uri)
        }
    }

    private fun onIpaPicked(uri: Uri) {
        setBusy(true)
        fileText.text = getString(R.string.sign_copying)
        Thread {
            val copied = copyPickedIpa(uri)
            runOnUiThread {
                setBusy(false)
                if (copied == null) {
                    fileText.text = getString(R.string.sign_copy_failed)
                    AppLog.write(filesDir, "安裝：複製所選 IPA 失敗")
                } else {
                    pickedFile = copied
                    fileText.text = getString(R.string.install_file_info, copied.name)
                    installButton.isEnabled = true
                    AppLog.write(filesDir, "安裝：已選 IPA ${copied.name}（${copied.length()} bytes）")
                }
            }
        }.start()
    }

    private fun displayName(uri: Uri): String {
        var name: String? = null
        try {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (idx >= 0 && cursor.moveToFirst()) {
                    name = cursor.getString(idx)
                }
            }
        } catch (e: Exception) {
            // fall through to the default name
        }
        return name ?: "app.ipa"
    }

    private fun copyPickedIpa(uri: Uri): File? {
        val rawName = displayName(uri)
        val clean = rawName.replace(Regex("[^A-Za-z0-9._-]"), "_")
        val fileName = if (clean.endsWith(".ipa", ignoreCase = true)) clean else "$clean.ipa"
        val dir = File(filesDir, "install_input").apply { mkdirs() }
        val out = File(dir, fileName)
        return try {
            contentResolver.openInputStream(uri)?.use { input ->
                out.outputStream().use { output -> input.copyTo(output) }
            } ?: return null
            out
        } catch (e: Exception) {
            null
        }
    }

    // -- Installation -----------------------------------------------------------

    /** Called from Rust worker threads while installation progresses. */
    fun onInstallProgress(percent: Int) {
        runOnUiThread {
            progressBar.progress = percent
            percentText.text = getString(R.string.install_progress, percent)
        }
    }

    private fun startInstall() {
        val input = pickedFile ?: return
        if (busy) return
        val subnet = localSubnet()
        if (subnet == null) {
            resultText.text = getString(R.string.install_no_wifi)
            return
        }
        setBusy(true)
        resultText.text = getString(R.string.install_working)
        progressBar.progress = 0
        progressBar.visibility = View.VISIBLE
        percentText.text = getString(R.string.install_progress, 0)
        percentText.visibility = View.VISIBLE
        AppLog.write(filesDir, "安裝：開始（${input.name}）")
        Thread {
            val result = try {
                nativeInstallIpa(
                    input.absolutePath,
                    subnet.first,
                    subnet.second,
                    filesDir.absolutePath
                )
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onInstallResult(result) }
        }.start()
    }

    private fun onInstallResult(result: String) {
        setBusy(false)
        val parts = result.split("\n", limit = 4)
        when (parts[0]) {
            "OK" -> {
                progressBar.progress = 100
                percentText.text = getString(R.string.install_progress, 100)
                resultText.text = getString(
                    R.string.install_done,
                    parts.getOrElse(2) { "" },
                    parts.getOrElse(3) { "" }
                )
                AppLog.write(filesDir, "安裝：完成（${parts.getOrElse(2) { "" }}）")
            }
            else -> {
                val code = parts.getOrElse(1) { "UNKNOWN" }
                val message = parts.getOrElse(2) { "" }
                resultText.text = when (code) {
                    "NOPAIRING" -> getString(R.string.install_device_missing)
                    "NOTFOUND" -> getString(R.string.install_notfound)
                    "BADIP" -> getString(R.string.install_no_wifi)
                    "PANIC" -> getString(R.string.error_panic)
                    else -> getString(R.string.install_failed, message)
                }
                AppLog.write(filesDir, "安裝：失敗（$code）")
            }
        }
    }

    companion object {
        private const val REQ_PICK_IPA = 4201

        init {
            System.loadLibrary("ipaloader_core")
        }
    }
}
