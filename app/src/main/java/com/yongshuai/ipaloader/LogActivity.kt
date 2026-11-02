package com.yongshuai.ipaloader

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Typeface
import android.os.Bundle
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

/**
 * Read-only viewer for the shared diagnostic log
 * (`<filesDir>/core/ipaloader.log`, written by both the Kotlin UI and
 * the Rust core). Holo dark theme comes from the application theme.
 */
class LogActivity : Activity() {

    private lateinit var logText: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        title = getString(R.string.log_page_title)

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(24, 24, 24, 24)
        }

        val buttonRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
        }
        val copyButton = Button(this).apply {
            text = getString(R.string.copy_all)
            setOnClickListener { copyAll() }
        }
        val clearButton = Button(this).apply {
            text = getString(R.string.clear_log)
            setOnClickListener {
                AppLog.clear(filesDir)
                refresh()
            }
        }
        buttonRow.addView(copyButton)
        buttonRow.addView(clearButton)

        logText = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            textSize = 12f
            setTextIsSelectable(true)
        }
        val scroll = ScrollView(this).apply {
            addView(logText)
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                0,
                1f
            )
        }

        layout.addView(buttonRow)
        layout.addView(scroll)
        setContentView(layout)

        refresh()
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun refresh() {
        val content = AppLog.read(filesDir)
        logText.text = if (content.isEmpty()) getString(R.string.log_empty) else content
        logText.gravity = Gravity.START
    }

    private fun copyAll() {
        val content = AppLog.read(filesDir)
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("ipaloader log", content))
        Toast.makeText(this, getString(R.string.log_copied), Toast.LENGTH_SHORT).show()
    }
}
