package com.yongshuai.ipaloader

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import java.io.File

/**
 * 0.6.2: the import-IPA tab. Pick an IPA once (the same document
 * picker + copy-into-app-storage flow the sign and install pages
 * use), then hand the stored file over to the sign tab or the
 * install tab, which take it from there with their existing
 * flows. This panel adds no processing of its own.
 */
class ImportPanel(
    private val host: Activity,
    private val onSendToSign: (File) -> Unit,
    private val onSendToInstall: (File) -> Unit
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

    private val fileText: TextView
    private val signButton: Button
    private val installButton: Button

    private var pickedFile: File? = null
    private var busy = false

    /** The panel's view hierarchy, built once and kept by the host. */
    val root: View

    init {
        val layout = LinearLayout(host).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }
        val pickButton = Button(host).apply {
            text = getString(R.string.sign_pick)
            setOnClickListener { pickIpa() }
        }
        fileText = TextView(host).apply {
            textSize = 15f
            text = getString(R.string.sign_no_file)
        }
        signButton = Button(host).apply {
            text = getString(R.string.import_to_sign)
            isEnabled = false
            setOnClickListener {
                val file = pickedFile ?: return@setOnClickListener
                onSendToSign(file)
            }
        }
        installButton = Button(host).apply {
            text = getString(R.string.import_to_install)
            isEnabled = false
            setOnClickListener {
                val file = pickedFile ?: return@setOnClickListener
                onSendToInstall(file)
            }
        }
        layout.addView(pickButton)
        layout.addView(fileText)
        layout.addView(signButton)
        layout.addView(installButton)
        root = layout
    }

    private var opened = false

    /** First-display hook, mirroring the old page-open behavior. */
    fun onFirstShown() {
        if (opened) return
        opened = true
        AppLog.write(filesDir, "開啟導入頁")
    }

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
        busy = true
        fileText.text = getString(R.string.sign_copying)
        signButton.isEnabled = false
        installButton.isEnabled = false
        Thread {
            val copied = copyPickedIpa(uri)
            runOnUiThread {
                busy = false
                if (copied == null) {
                    fileText.text = getString(R.string.sign_copy_failed)
                    AppLog.write(filesDir, "導入：複製所選 IPA 失敗")
                } else {
                    pickedFile = copied
                    fileText.text = getString(R.string.install_file_info, copied.name)
                    signButton.isEnabled = true
                    installButton.isEnabled = true
                    AppLog.write(
                        filesDir,
                        "導入：已選 IPA ${copied.name}（${copied.length()} bytes）"
                    )
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
        val dir = File(filesDir, "import_input").apply { mkdirs() }
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

    companion object {
        private const val REQ_PICK_IPA = 4401
    }
}
