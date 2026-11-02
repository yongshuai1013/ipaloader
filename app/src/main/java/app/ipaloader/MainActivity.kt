package app.ipaloader

import android.app.Activity
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

/**
 * M1 shell: Holo (dark) native UI. Shows the string returned by the Rust
 * core through JNI to prove the Kotlin <-> Rust channel works. The login
 * button stays disabled until M2 wires up the real login flow.
 */
class MainActivity : Activity() {

    external fun nativeVersion(): String

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
            textSize = 18f
        }

        val loginButton = Button(this).apply {
            text = getString(R.string.login)
            isEnabled = false // enabled in M2
        }

        layout.addView(versionText)
        layout.addView(loginButton)
        setContentView(layout)
    }

    companion object {
        init {
            System.loadLibrary("ipaloader_core")
        }
    }
}
