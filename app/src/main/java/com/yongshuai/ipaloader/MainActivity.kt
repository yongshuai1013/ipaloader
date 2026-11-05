package com.yongshuai.ipaloader

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * M2: real Apple ID login. Holo (dark) native UI; all blocking native
 * calls run on background threads. The Rust core calls back into
 * [onTwoFactorNeeded] on a native thread when Apple asks for a 2FA code.
 */
class MainActivity : Activity() {

    external fun nativeVersion(): String
    external fun nativeLogin(email: String, password: String, filesDir: String): String
    external fun nativeRestoreSession(filesDir: String): String
    external fun nativeLogout(filesDir: String): Boolean
    external fun nativeListInventory(filesDir: String): String
    external fun nativeExportSigningKey(filesDir: String, outPath: String): String
    external fun nativeImportSigningKey(srcPath: String, filesDir: String): String

    private lateinit var statusText: TextView
    private lateinit var progressBar: ProgressBar
    private lateinit var emailField: EditText
    private lateinit var passwordField: EditText
    private lateinit var rememberCheck: CheckBox
    private lateinit var loginButton: Button
    private lateinit var logButton: Button
    private lateinit var accountText: TextView
    private lateinit var teamText: TextView
    private lateinit var logoutButton: Button
    private lateinit var loginForm: LinearLayout
    private lateinit var accountBox: LinearLayout

    // 0.6.2 tab menu (always visible, independent of login state).
    private lateinit var signPanel: SignActivity
    private lateinit var installPanel: InstallActivity
    private lateinit var pairingPanel: PairingPanel
    private lateinit var importPanel: ImportPanel
    private lateinit var inventoryPanel: InventoryPanel
    private val tabLabels = mutableListOf<TextView>()
    private val tabIndicators = mutableListOf<View>()
    private var selectedTab = -1

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
        }

        val versionText = TextView(this).apply {
            text = try {
                nativeVersion()
            } catch (e: UnsatisfiedLinkError) {
                getString(R.string.rust_core_missing)
            }
            textSize = 14f
        }

        statusText = TextView(this).apply { textSize = 16f }
        progressBar = ProgressBar(this).apply { visibility = View.GONE }

        emailField = EditText(this).apply {
            hint = getString(R.string.email_hint)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS
        }
        passwordField = EditText(this).apply {
            hint = getString(R.string.password_hint)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        rememberCheck = CheckBox(this).apply {
            text = getString(R.string.remember_account)
            isChecked = true
        }
        loginButton = Button(this).apply {
            text = getString(R.string.login)
            setOnClickListener { startLogin() }
        }
        loginForm = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(emailField)
            addView(passwordField)
            addView(rememberCheck)
            addView(loginButton)
        }

        logButton = Button(this).apply {
            text = getString(R.string.view_log)
            setOnClickListener {
                startActivity(Intent(this@MainActivity, LogActivity::class.java))
            }
        }

        accountText = TextView(this).apply { textSize = 18f }
        teamText = TextView(this).apply { textSize = 16f }
        logoutButton = Button(this).apply {
            text = getString(R.string.logout)
            setOnClickListener { startLogout() }
        }
        accountBox = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            addView(accountText)
            addView(teamText)
            addView(logoutButton)
        }

        layout.addView(versionText)
        layout.addView(statusText)
        layout.addView(progressBar)
        layout.addView(logButton)
        layout.addView(loginForm)
        layout.addView(accountBox)

        // -- Tab menu ---------------------------------------------------
        // Always on screen, below the login block, and never gated by
        // login state: the panels manage their own prerequisites
        // (e.g. the sign flow stops with NO_SESSION) exactly as the
        // standalone pages did.
        signPanel = SignActivity(this) { file ->
            selectTab(TAB_INSTALL)
            installPanel.prePick(file, file.name)
        }
        installPanel = InstallActivity(this)
        pairingPanel = PairingPanel(this, signPanel)
        importPanel = ImportPanel(
            this,
            onSendToSign = { file ->
                selectTab(TAB_SIGN)
                signPanel.acceptExternalIpa(file)
            },
            onSendToInstall = { file ->
                selectTab(TAB_INSTALL)
                installPanel.prePick(file, file.name)
            }
        )
        inventoryPanel = InventoryPanel(this)

        val tabRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
        }
        tabRow.addView(makeTab(TAB_SIGN, R.string.tab_sign))
        tabRow.addView(makeTab(TAB_INSTALL, R.string.tab_install))
        tabRow.addView(makeTab(TAB_PAIRING, R.string.tab_pairing))
        tabRow.addView(makeTab(TAB_IMPORT, R.string.tab_import))
        tabRow.addView(makeTab(TAB_INVENTORY, R.string.tab_inventory))
        val tabScroll = HorizontalScrollView(this).apply {
            isHorizontalScrollBarEnabled = false
            setPadding(0, 24, 0, 0)
            addView(tabRow)
        }
        layout.addView(tabScroll)

        val contentBox = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(signPanel.root)
            addView(installPanel.root)
            addView(pairingPanel.root)
            addView(importPanel.root)
            addView(inventoryPanel.root)
        }
        val contentScroll = ScrollView(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                0,
                1f
            )
            addView(contentBox)
        }
        layout.addView(contentScroll)
        setContentView(layout)
        selectTab(TAB_SIGN)

        prefillSavedCredentials()

        AppLog.write(filesDir, "App 啟動，開始恢復工作階段")

        setBusy(true, getString(R.string.status_checking))
        Thread {
            val result = try {
                nativeRestoreSession(filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "NONE"
            }
            runOnUiThread { onRestoreResult(result) }
        }.start()
    }

    // -- Tab menu ---------------------------------------------------------

    /** One text tab with a bottom indicator, Holo-dark styled. */
    private fun makeTab(index: Int, labelRes: Int): View {
        val indicator = View(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                4
            )
            setBackgroundColor(Color.rgb(0x33, 0x33, 0x33))
        }
        val label = TextView(this).apply {
            text = getString(labelRes)
            textSize = 15f
            gravity = Gravity.CENTER
            setTextColor(Color.rgb(0xAA, 0xAA, 0xAA))
            setPadding(36, 16, 36, 16)
        }
        tabLabels.add(label)
        tabIndicators.add(indicator)
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(label)
            addView(indicator)
            setOnClickListener { selectTab(index) }
        }
    }

    private fun selectTab(index: Int) {
        selectedTab = index
        val roots = listOf(
            signPanel.root,
            installPanel.root,
            pairingPanel.root,
            importPanel.root,
            inventoryPanel.root
        )
        roots.forEachIndexed { i, root ->
            root.visibility = if (i == index) View.VISIBLE else View.GONE
        }
        tabLabels.forEachIndexed { i, tv ->
            tv.setTextColor(
                if (i == index) Color.WHITE else Color.rgb(0xAA, 0xAA, 0xAA)
            )
        }
        tabIndicators.forEachIndexed { i, v ->
            v.setBackgroundColor(
                if (i == index) {
                    Color.rgb(0x33, 0xB5, 0xE5)
                } else {
                    Color.rgb(0x33, 0x33, 0x33)
                }
            )
        }
        when (index) {
            TAB_SIGN -> signPanel.onFirstShown()
            TAB_INSTALL -> installPanel.onFirstShown()
            TAB_PAIRING -> pairingPanel.onFirstShown()
            TAB_IMPORT -> importPanel.onFirstShown()
            TAB_INVENTORY -> inventoryPanel.onFirstShown()
        }
    }

    /** Document-picker results are routed to the tab panels. */
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (::signPanel.isInitialized) {
            signPanel.onActivityResult(requestCode, resultCode, data)
        }
        if (::installPanel.isInitialized) {
            installPanel.onActivityResult(requestCode, resultCode, data)
        }
        if (::pairingPanel.isInitialized) {
            pairingPanel.onActivityResult(requestCode, resultCode, data)
        }
        if (::importPanel.isInitialized) {
            importPanel.onActivityResult(requestCode, resultCode, data)
        }
        if (::inventoryPanel.isInitialized) {
            inventoryPanel.onActivityResult(requestCode, resultCode, data)
        }
    }

    private fun setBusy(busy: Boolean, status: String) {
        progressBar.visibility = if (busy) View.VISIBLE else View.GONE
        statusText.text = status
        loginButton.isEnabled = !busy
        emailField.isEnabled = !busy
        passwordField.isEnabled = !busy
        rememberCheck.isEnabled = !busy
        logoutButton.isEnabled = !busy
    }

    private fun showLoggedIn(email: String, team: String) {
        loginForm.visibility = View.GONE
        accountBox.visibility = View.VISIBLE
        accountText.text = getString(R.string.logged_in_as, email)
        teamText.text = if (team.isNotEmpty()) {
            getString(R.string.team_label, team)
        } else {
            getString(R.string.team_unknown)
        }
        setBusy(false, getString(R.string.status_logged_in))
    }

    private fun showLoginForm(status: String, prefill: Boolean = true) {
        accountBox.visibility = View.GONE
        loginForm.visibility = View.VISIBLE
        if (prefill) prefillSavedCredentials()
        setBusy(false, status)
    }

    /** Fills the login form from [CredentialStore] when a pair is saved. */
    private fun prefillSavedCredentials() {
        val saved = CredentialStore.load(this) ?: return
        emailField.setText(saved.first)
        passwordField.setText(saved.second)
        rememberCheck.isChecked = true
    }

    private fun startLogin() {
        val email = emailField.text.toString().trim()
        val password = passwordField.text.toString()
        if (email.isEmpty() || password.isEmpty()) {
            statusText.text = getString(R.string.error_empty_fields)
            return
        }
        AppLog.write(filesDir, "按下登入：$email")
        setBusy(true, getString(R.string.status_logging_in))
        Thread {
            val result = try {
                nativeLogin(email, password, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread {
                passwordField.text.clear()
                onLoginResult(result, email, password)
            }
        }.start()
    }

    private fun startLogout() {
        AppLog.write(filesDir, "按下登出")
        setBusy(true, getString(R.string.status_logging_out))
        Thread {
            try {
                nativeLogout(filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                // nothing to do
            }
            runOnUiThread { showLoginForm(getString(R.string.status_logged_out)) }
        }.start()
    }

    private fun onRestoreResult(result: String) {
        val parts = result.split("\n", limit = 3)
        AppLog.write(
            filesDir,
            "工作階段恢復結果：${parts[0]}" +
                if (parts[0] == "ERR") "（${parts.getOrElse(1) { "UNKNOWN" }}）" else ""
        )
        when (parts[0]) {
            "OK" -> showLoggedIn(parts.getOrElse(1) { "" }, parts.getOrElse(2) { "" })
            "ERR" -> showLoginForm(
                getString(R.string.status_restore_failed, friendlyError(parts.getOrElse(1) { "UNKNOWN" }, parts.getOrElse(2) { "" }))
            )
            else -> showLoginForm(getString(R.string.status_please_login))
        }
    }

    private fun onLoginResult(result: String, email: String, password: String) {
        val parts = result.split("\n", limit = 3)
        AppLog.write(
            filesDir,
            if (parts[0] == "OK") {
                "登入結果：OK"
            } else {
                "登入結果：ERR（${parts.getOrElse(1) { "UNKNOWN" }}）"
            }
        )
        when (parts[0]) {
            "OK" -> {
                // Remember-me: only a successful login touches the
                // saved pair. Checked -> save/refresh it; unchecked ->
                // forget any previously saved pair. The values
                // themselves are never logged.
                if (rememberCheck.isChecked) {
                    CredentialStore.save(this, email, password)
                    AppLog.write(filesDir, "已記住帳號（密碼加密保存）")
                } else {
                    CredentialStore.clear(this)
                    AppLog.write(filesDir, "未勾選記住帳號，已清除先前保存")
                }
                showLoggedIn(parts.getOrElse(1) { "" }, parts.getOrElse(2) { "" })
            }
            else -> {
                val code = parts.getOrElse(1) { "UNKNOWN" }
                val message = parts.getOrElse(2) { "" }
                showLoginForm(friendlyError(code, message), prefill = false)
            }
        }
    }

    private fun friendlyError(code: String, message: String): String {
        return when (code) {
            "PANIC" -> getString(R.string.error_panic)
            "BAD_CREDENTIALS" -> getString(R.string.error_bad_credentials)
            "CANCELLED" -> getString(R.string.error_cancelled)
            "NETWORK" -> getString(R.string.error_network)
            "ANISETTE" -> getString(R.string.error_anisette, message)
            else -> getString(R.string.error_unknown, message)
        }
    }

    /**
     * Called from a Rust worker thread while login is in progress.
     * Shows a Holo dialog on the UI thread and blocks until the user
     * answers. [kind] is "device", "sms" or "unknown"; [numbers] is a
     * comma separated list of "id:lastTwoDigits" trusted phone numbers;
     * [lastError] is an optional server message from the previous try.
     * Returns "code:<code>", "sms:<id>", "devices", "resend" or "abort".
     * Never logs the code.
     */
    fun onTwoFactorNeeded(kind: String, numbers: String, lastError: String): String {
        if (isFinishing || isDestroyed) return "abort"
        val numberList = numbers.split(",")
            .filter { it.contains(":") }
            .map {
                val p = it.split(":", limit = 2)
                p[0] to p[1]
            }
        val result = AtomicReference("abort")
        val latch = CountDownLatch(1)
        runOnUiThread {
            try {
                showTwoFactorDialog(kind, numberList, lastError, result, latch)
            } catch (e: Exception) {
                latch.countDown()
            }
        }
        AppLog.write(filesDir, "2FA 對話框顯示：kind=$kind")
        latch.await(300, TimeUnit.SECONDS)
        val answer = result.get()
        // Log only the chosen action, never the submitted code.
        val action = when {
            answer.startsWith("code:") -> "提交驗證碼"
            answer.startsWith("sms:") -> "選擇簡訊號碼"
            answer == "resend" -> "重新發送"
            answer == "devices" -> "傳送到信任裝置"
            else -> "取消"
        }
        AppLog.write(filesDir, "2FA 對話框結果：$action")
        return answer
    }

    private fun showTwoFactorDialog(
        kind: String,
        numberList: List<Pair<String, String>>,
        lastError: String,
        result: AtomicReference<String>,
        latch: CountDownLatch
    ) {
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 32, 48, 16)
        }
        if (lastError.isNotEmpty()) {
            content.addView(TextView(this).apply {
                text = getString(R.string.tfa_last_error, lastError)
            })
        }
        content.addView(TextView(this).apply {
            text = getString(
                when (kind) {
                    "sms" -> R.string.tfa_prompt_sms
                    "unknown" -> R.string.tfa_prompt_unknown
                    else -> R.string.tfa_prompt_device
                }
            )
        })

        val codeField = EditText(this).apply {
            hint = getString(R.string.tfa_code_hint)
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        if (kind != "unknown") {
            content.addView(codeField)
        }

        val dialog = AlertDialog.Builder(this)
            .setTitle(
                when (kind) {
                    "sms" -> R.string.tfa_title_sms
                    "unknown" -> R.string.tfa_title_unknown
                    else -> R.string.tfa_title_device
                }
            )
            .setView(content)
            .setCancelable(false)
            .create()

        fun answer(value: String) {
            result.set(value)
            dialog.dismiss()
            latch.countDown()
        }
        fun addAction(label: String, value: () -> String) {
            content.addView(Button(this).apply {
                text = label
                setOnClickListener { answer(value()) }
            })
        }

        if (kind != "unknown") {
            addAction(getString(R.string.tfa_submit)) { "code:" + codeField.text.toString().trim() }
            addAction(getString(R.string.tfa_resend)) { "resend" }
        } else {
            addAction(getString(R.string.tfa_send_devices)) { "devices" }
        }
        if (kind == "sms") {
            addAction(getString(R.string.tfa_send_devices)) { "devices" }
        }
        for ((id, lastTwo) in numberList) {
            addAction(getString(R.string.tfa_send_sms, lastTwo)) { "sms:$id" }
        }
        addAction(getString(R.string.cancel)) { "abort" }

        dialog.setOnDismissListener { latch.countDown() }
        dialog.show()
    }

    companion object {
        private const val TAB_SIGN = 0
        private const val TAB_INSTALL = 1
        private const val TAB_PAIRING = 2
        private const val TAB_IMPORT = 3
        private const val TAB_INVENTORY = 4

        init {
            System.loadLibrary("ipaloader_core")
        }
    }
}
