package com.yongshuai.ipaloader

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/**
 * M3: IPA signing page. Pick an IPA (system document picker), show
 * the target app's info (read by the Rust core), then sign it with
 * the restored login session: certificate -> App ID -> provisioning
 * profile -> bundle signing -> re-zip into an installable IPA under
 * `<filesDir>/signed/`, with an optional "save to…" copy afterwards.
 *
 * The Rust core reports stage keys through [onSignProgress]:
 * cert / appid / profile / sign / zip. A certificate-limit stop
 * comes back as a CERTLIMIT result carrying the existing
 * certificates; nothing is ever revoked automatically.
 */
class SignActivity : Activity() {

    external fun nativeInspectIpa(ipaPath: String, filesDir: String): String
    external fun nativeSignIpa(ipaPath: String, outPath: String, filesDir: String): String

    private lateinit var pickButton: Button
    private lateinit var infoText: TextView
    private lateinit var signButton: Button
    private lateinit var progressBar: ProgressBar
    private lateinit var resultText: TextView
    private lateinit var limitText: TextView
    private lateinit var saveButton: Button

    private val stageOrder = listOf("cert", "appid", "profile", "sign", "zip")
    private val stageLabels = LinkedHashMap<String, TextView>()
    private var stageReached = -1
    private var stageFailed = false

    private var pickedFile: File? = null
    private var pickedBaseName = "app"
    private var signedFile: File? = null
    private var busy = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = getString(R.string.sign_title)

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }

        pickButton = Button(this).apply {
            text = getString(R.string.sign_pick)
            setOnClickListener { pickIpa() }
        }
        infoText = TextView(this).apply {
            textSize = 15f
            text = getString(R.string.sign_no_file)
        }
        signButton = Button(this).apply {
            text = getString(R.string.sign_start)
            isEnabled = false
            setOnClickListener { startSign() }
        }
        progressBar = ProgressBar(this).apply { visibility = View.GONE }

        val stageTitle = TextView(this).apply {
            textSize = 16f
            setPadding(0, 32, 0, 8)
            text = getString(R.string.sign_stages_title)
        }
        layout.addView(pickButton)
        layout.addView(infoText)
        layout.addView(signButton)
        layout.addView(progressBar)
        layout.addView(stageTitle)
        val labelRes = mapOf(
            "cert" to R.string.sign_stage_cert,
            "appid" to R.string.sign_stage_appid,
            "profile" to R.string.sign_stage_profile,
            "sign" to R.string.sign_stage_sign,
            "zip" to R.string.sign_stage_zip
        )
        for (key in stageOrder) {
            val tv = TextView(this).apply { textSize = 15f }
            stageLabels[key] = tv
            layout.addView(tv)
        }
        for ((key, res) in labelRes) {
            stageLabels[key]?.tag = getString(res)
        }
        resetStages()

        resultText = TextView(this).apply {
            textSize = 15f
            setPadding(0, 32, 0, 0)
        }
        limitText = TextView(this).apply {
            textSize = 15f
            setPadding(0, 32, 0, 0)
            visibility = View.GONE
        }
        saveButton = Button(this).apply {
            text = getString(R.string.sign_save_as)
            visibility = View.GONE
            setOnClickListener { saveSigned() }
        }
        layout.addView(resultText)
        layout.addView(limitText)
        layout.addView(saveButton)

        setContentView(layout)
        AppLog.write(filesDir, "開啟簽名頁")
    }

    // -- Stage checklist ---------------------------------------------------

    private fun resetStages() {
        stageReached = -1
        stageFailed = false
        for ((_, tv) in stageLabels) {
            tv.text = "・${tv.tag}"
        }
    }

    private fun renderStages() {
        stageOrder.forEachIndexed { index, key ->
            val tv = stageLabels[key] ?: return@forEachIndexed
            val mark = when {
                stageFailed && index == stageReached -> "✗"
                index < stageReached -> "✓"
                index == stageReached -> "▶"
                else -> "・"
            }
            tv.text = "$mark ${tv.tag}"
        }
    }

    private fun applyStage(stage: String) {
        val index = stageOrder.indexOf(stage)
        if (index < 0 || index <= stageReached) return
        stageReached = index
        renderStages()
    }

    private fun completeStages() {
        stageReached = stageOrder.size
        renderStages()
    }

    private fun failStage() {
        stageFailed = true
        if (stageReached < 0) stageReached = 0
        renderStages()
    }

    /** Called from Rust worker threads while signing progresses. */
    fun onSignProgress(stage: String) {
        runOnUiThread { applyStage(stage) }
    }

    // -- Picking & inspecting ----------------------------------------------

    private fun pickIpa() {
        if (busy) return
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
        }
        startActivityForResult(intent, REQ_PICK_IPA)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != RESULT_OK) return
        val uri = data?.data ?: return
        when (requestCode) {
            REQ_PICK_IPA -> onIpaPicked(uri)
            REQ_SAVE_AS -> onSaveTargetChosen(uri)
        }
    }

    private fun onIpaPicked(uri: Uri) {
        setBusy(true)
        infoText.text = getString(R.string.sign_copying)
        signButton.isEnabled = false
        resultText.text = ""
        limitText.visibility = View.GONE
        saveButton.visibility = View.GONE
        signedFile = null
        resetStages()
        Thread {
            val copied = copyPickedIpa(uri)
            runOnUiThread {
                if (copied == null) {
                    setBusy(false)
                    infoText.text = getString(R.string.sign_copy_failed)
                    AppLog.write(filesDir, "簽名：複製所選 IPA 失敗")
                } else {
                    pickedFile = copied
                    inspect(copied)
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
        pickedBaseName = fileName.removeSuffix(".ipa").removeSuffix(".IPA")
        if (pickedBaseName.isEmpty()) pickedBaseName = "app"
        val dir = File(filesDir, "sign_input").apply { mkdirs() }
        val out = File(dir, fileName)
        return try {
            contentResolver.openInputStream(uri)?.use { input ->
                out.outputStream().use { output -> input.copyTo(output) }
            } ?: return null
            AppLog.write(filesDir, "簽名：已選 IPA $fileName（${out.length()} bytes）")
            out
        } catch (e: Exception) {
            null
        }
    }

    private fun inspect(file: File) {
        infoText.text = getString(R.string.sign_inspecting)
        Thread {
            val result = try {
                nativeInspectIpa(file.absolutePath, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onInspectResult(result) }
        }.start()
    }

    private fun onInspectResult(result: String) {
        setBusy(false)
        val parts = result.split("\n", limit = 4)
        when (parts[0]) {
            "OK" -> {
                infoText.text = getString(
                    R.string.sign_app_info,
                    parts.getOrElse(2) { "" },
                    parts.getOrElse(1) { "" },
                    parts.getOrElse(3) { "" }
                )
                signButton.isEnabled = true
                AppLog.write(filesDir, "簽名：IPA 資訊讀取完成（${parts.getOrElse(1) { "" }}）")
            }
            else -> {
                infoText.text = getString(
                    R.string.sign_inspect_failed,
                    parts.getOrElse(2) { "" }
                )
                signButton.isEnabled = false
                AppLog.write(filesDir, "簽名：IPA 資訊讀取失敗（${parts.getOrElse(1) { "?" }}）")
            }
        }
    }

    // -- Signing -------------------------------------------------------------

    private fun setBusy(value: Boolean) {
        busy = value
        progressBar.visibility = if (value) View.VISIBLE else View.GONE
        pickButton.isEnabled = !value
        if (value) signButton.isEnabled = false
    }

    private fun startSign() {
        val input = pickedFile ?: return
        if (busy) return
        val outFile = File(File(filesDir, "signed"), "$pickedBaseName-signed.ipa")
        setBusy(true)
        resultText.text = getString(R.string.sign_working)
        limitText.visibility = View.GONE
        saveButton.visibility = View.GONE
        signedFile = null
        resetStages()
        applyStage("cert")
        AppLog.write(filesDir, "簽名：開始（$pickedBaseName）")
        Thread {
            val result = try {
                nativeSignIpa(input.absolutePath, outFile.absolutePath, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onSignResult(result, outFile) }
        }.start()
    }

    private fun onSignResult(result: String, outFile: File) {
        setBusy(false)
        signButton.isEnabled = pickedFile != null
        val head = result.substringBefore("\n")
        when (head) {
            "OK" -> {
                val parts = result.split("\n", limit = 4)
                completeStages()
                signedFile = outFile
                val expiry = formatExpiry(parts.getOrElse(2) { "" })
                var text = getString(R.string.sign_done_path, parts.getOrElse(1) { outFile.absolutePath })
                text += "\n" + getString(R.string.sign_expiry, expiry)
                val newBundle = parts.getOrElse(3) { "" }
                if (newBundle.isNotEmpty()) {
                    text += "\n" + getString(R.string.sign_new_bundle, newBundle)
                }
                resultText.text = text
                saveButton.visibility = View.VISIBLE
                AppLog.write(filesDir, "簽名：完成，描述檔到期 $expiry")
            }
            "CERTLIMIT" -> {
                failStage()
                showCertLimit(result.substringAfter("\n", ""))
                AppLog.write(filesDir, "簽名：因證書達上限停止（未撤銷任何證書）")
            }
            else -> {
                failStage()
                val parts = result.split("\n", limit = 3)
                val code = parts.getOrElse(1) { "UNKNOWN" }
                val message = parts.getOrElse(2) { "" }
                resultText.text = when (code) {
                    "NO_SESSION" -> getString(R.string.sign_no_session)
                    "PANIC" -> getString(R.string.error_panic)
                    "NETWORK" -> getString(R.string.error_network)
                    "ANISETTE" -> getString(R.string.error_anisette, message)
                    else -> getString(R.string.sign_failed, message)
                }
                AppLog.write(filesDir, "簽名：失敗（$code）")
            }
        }
    }

    private fun showCertLimit(json: String) {
        val lines = StringBuilder()
        lines.append(getString(R.string.sign_cert_limit_title)).append("\n")
        lines.append(getString(R.string.sign_cert_limit_body)).append("\n")
        try {
            val array = JSONArray(json)
            for (i in 0 until array.length()) {
                val cert = array.getJSONObject(i)
                val name = cert.optString("name")
                val machine = cert.optString("machineName")
                val expires = formatExpiry(cert.optString("expires")).substringBefore(" ")
                lines.append(getString(R.string.sign_cert_line, name, machine, expires))
                if (i < array.length() - 1) lines.append("\n")
            }
        } catch (e: Exception) {
            // The server-side list could not be parsed; the guidance
            // text above still tells the user what to do.
        }
        limitText.text = lines.toString()
        limitText.visibility = View.VISIBLE
        resultText.text = ""
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

    // -- Save a copy ---------------------------------------------------------

    private fun saveSigned() {
        val file = signedFile ?: return
        val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "application/octet-stream"
            putExtra(Intent.EXTRA_TITLE, file.name)
        }
        startActivityForResult(intent, REQ_SAVE_AS)
    }

    private fun onSaveTargetChosen(uri: Uri) {
        val file = signedFile ?: return
        Thread {
            val ok = try {
                contentResolver.openOutputStream(uri)?.use { output ->
                    file.inputStream().use { input -> input.copyTo(output) }
                } != null
            } catch (e: Exception) {
                false
            }
            runOnUiThread {
                if (ok) {
                    Toast.makeText(this, getString(R.string.sign_saved), Toast.LENGTH_SHORT).show()
                    AppLog.write(filesDir, "簽名：已另存已簽 IPA")
                }
            }
        }.start()
    }

    companion object {
        private const val REQ_PICK_IPA = 4101
        private const val REQ_SAVE_AS = 4102

        init {
            System.loadLibrary("ipaloader_core")
        }
    }
}
