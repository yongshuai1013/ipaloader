package app.ipaloader

import android.app.Activity
import android.app.AlertDialog
import android.os.Bundle
import android.text.InputType
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ProgressBar
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

    private lateinit var statusText: TextView
    private lateinit var progressBar: ProgressBar
    private lateinit var emailField: EditText
    private lateinit var passwordField: EditText
    private lateinit var loginButton: Button
    private lateinit var accountText: TextView
    private lateinit var teamText: TextView
    private lateinit var logoutButton: Button
    private lateinit var loginForm: LinearLayout
    private lateinit var accountBox: LinearLayout

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
        loginButton = Button(this).apply {
            text = getString(R.string.login)
            setOnClickListener { startLogin() }
        }
        loginForm = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(emailField)
            addView(passwordField)
            addView(loginButton)
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
        layout.addView(loginForm)
        layout.addView(accountBox)
        setContentView(layout)

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

    private fun setBusy(busy: Boolean, status: String) {
        progressBar.visibility = if (busy) View.VISIBLE else View.GONE
        statusText.text = status
        loginButton.isEnabled = !busy
        emailField.isEnabled = !busy
        passwordField.isEnabled = !busy
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

    private fun showLoginForm(status: String) {
        accountBox.visibility = View.GONE
        loginForm.visibility = View.VISIBLE
        setBusy(false, status)
    }

    private fun startLogin() {
        val email = emailField.text.toString().trim()
        val password = passwordField.text.toString()
        if (email.isEmpty() || password.isEmpty()) {
            statusText.text = getString(R.string.error_empty_fields)
            return
        }
        setBusy(true, getString(R.string.status_logging_in))
        Thread {
            val result = try {
                nativeLogin(email, password, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread {
                passwordField.text.clear()
                onLoginResult(result)
            }
        }.start()
    }

    private fun startLogout() {
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
        when (parts[0]) {
            "OK" -> showLoggedIn(parts.getOrElse(1) { "" }, parts.getOrElse(2) { "" })
            "ERR" -> showLoginForm(
                getString(R.string.status_restore_failed, friendlyError(parts.getOrElse(1) { "UNKNOWN" }, parts.getOrElse(2) { "" }))
            )
            else -> showLoginForm(getString(R.string.status_please_login))
        }
    }

    private fun onLoginResult(result: String) {
        val parts = result.split("\n", limit = 3)
        when (parts[0]) {
            "OK" -> showLoggedIn(parts.getOrElse(1) { "" }, parts.getOrElse(2) { "" })
            else -> {
                val code = parts.getOrElse(1) { "UNKNOWN" }
                val message = parts.getOrElse(2) { "" }
                showLoginForm(friendlyError(code, message))
            }
        }
    }

    private fun friendlyError(code: String, message: String): String {
        return when (code) {
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
        latch.await(300, TimeUnit.SECONDS)
        return result.get()
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
        init {
            System.loadLibrary("ipaloader_core")
        }
    }
}
