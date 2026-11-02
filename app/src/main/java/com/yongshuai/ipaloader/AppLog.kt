package com.yongshuai.ipaloader

import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * Appends UI-side events to the same diagnostic log the Rust core
 * writes: `<filesDir>/core/ipaloader.log`. One timestamped line per
 * event. Never call this with the password, a 2FA code, or any
 * session/token content. Best-effort: logging must never crash the app.
 */
object AppLog {

    private val lock = Any()
    private val format = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US)

    private fun logFile(filesDir: File): File = File(File(filesDir, "core"), "ipaloader.log")

    fun write(filesDir: File, message: String) {
        synchronized(lock) {
            try {
                val file = logFile(filesDir)
                file.parentFile?.mkdirs()
                val flat = message.replace("\n", " ")
                file.appendText("[${format.format(Date())}] [app] $flat\n")
            } catch (e: Exception) {
                // ignore: diagnostics only
            }
        }
    }

    fun read(filesDir: File): String {
        return try {
            val file = logFile(filesDir)
            if (file.exists()) file.readText() else ""
        } catch (e: Exception) {
            ""
        }
    }

    fun clear(filesDir: File) {
        synchronized(lock) {
            try {
                logFile(filesDir).delete()
            } catch (e: Exception) {
                // ignore
            }
        }
    }
}
