package com.yongshuai.ipaloader

import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint

/**
 * 0.6.3: raw bulk pipe to the iPhone's usbmux interface,
 * driven from Rust through JNI (called on the mux IO thread
 * only). The Rust side speaks the mux protocol; this class
 * only moves bytes.
 *
 * Contract the Rust bridge relies on:
 * - [usbWrite] writes the whole buffer (looping over partial
 *   transfers) and returns its length, or -1 on failure.
 * - [usbRead] returns the bytes of one transfer; an empty
 *   array on timeout; null on hard failure.
 * - [usbNote] is a one-line description for the app log.
 */
class UsbMuxLink(
    private val connection: UsbDeviceConnection,
    private val endpointIn: UsbEndpoint,
    private val endpointOut: UsbEndpoint,
    private val note: String
) {
    @Volatile
    private var closed = false

    fun usbNote(): String = note

    fun usbWrite(data: ByteArray): Int {
        if (closed) return -1
        var offset = 0
        while (offset < data.size) {
            val chunk = minOf(data.size - offset, WRITE_CHUNK)
            val n = try {
                connection.bulkTransfer(
                    endpointOut, data, offset, chunk, WRITE_TIMEOUT_MS
                )
            } catch (e: Exception) {
                -1
            }
            if (n <= 0) return -1
            offset += n
        }
        return data.size
    }

    fun usbRead(max: Int, timeoutMs: Int): ByteArray? {
        if (closed) return null
        val buffer = ByteArray(max)
        val n = try {
            connection.bulkTransfer(endpointIn, buffer, buffer.size, timeoutMs)
        } catch (e: Exception) {
            -1
        }
        return when {
            n > 0 -> buffer.copyOf(n)
            // -1 with a positive timeout is Android's timeout
            // signal; the Rust poll loop treats it as "no data
            // yet" and retries, while null is reserved for a
            // torn-down link (see close()).
            n == -1 && timeoutMs > 0 && !closed -> ByteArray(0)
            closed -> null
            else -> ByteArray(0)
        }
    }

    fun close() {
        closed = true
    }

    private companion object {
        const val WRITE_CHUNK = 16384
        const val WRITE_TIMEOUT_MS = 5000
    }
}
