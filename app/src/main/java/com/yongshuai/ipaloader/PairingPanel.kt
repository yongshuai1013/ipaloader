package com.yongshuai.ipaloader

import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConfiguration
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.net.Uri
import android.os.Build
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
 *
 * 0.6.3: below the import block, the panel can also pair with a
 * USB-connected device directly (the only way a first pairing
 * can happen — Wi-Fi lockdownd needs an existing trust
 * relationship). This class does the Android USB host dance
 * (enumerate, permission, open, pick the usbmux interface) and
 * hands the bulk pipe ([UsbMuxLink]) to the Rust mux client,
 * which runs the protocol and lockdownd `Pair`.
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
    private val usbStatus: TextView
    private val usbButton: Button

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
        val spacer = View(host)
        layout.addView(spacer, LinearLayout.LayoutParams(1, 48))
        val usbTitle = TextView(host).apply {
            textSize = 16f
            text = getString(R.string.pair_usb_title)
        }
        val usbHint = TextView(host).apply {
            textSize = 13f
            text = getString(R.string.pair_usb_hint)
        }
        usbStatus = TextView(host).apply { textSize = 15f }
        usbButton = Button(host).apply {
            text = getString(R.string.pair_usb_button)
            setOnClickListener { startUsbPair() }
        }
        layout.addView(usbTitle)
        layout.addView(usbHint)
        layout.addView(usbStatus)
        layout.addView(usbButton)
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

    // --------------------------------------------------------
    // 0.6.3: USB pairing (Android host side; the protocol and
    // the lockdownd Pair handshake run in the Rust core)
    // --------------------------------------------------------

    /** One candidate usbmux interface on the device. */
    private class MuxCandidate(
        val config: UsbConfiguration,
        val iface: UsbInterface,
        val endpointIn: UsbEndpoint,
        val endpointOut: UsbEndpoint,
        val strict: Boolean
    ) {
        val configValue: Int get() = config.id
    }

    private var usbRunning = false
    private var usbManager: UsbManager? = null
    private var usbConnection: UsbDeviceConnection? = null
    private var usbClaimed: UsbInterface? = null
    private var usbReceiverRegistered = false

    private val usbPermissionReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != ACTION_USB_PERMISSION) return
            unregisterUsbReceiver()
            @Suppress("DEPRECATION")
            val device = intent.getParcelableExtra(UsbManager.EXTRA_DEVICE) as? UsbDevice
            val granted =
                intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
            if (granted && device != null) {
                openAndPair(device)
            } else {
                AppLog.write(filesDir, "USB 配對：使用者未授權 USB 存取")
                showUsbError("USBPERM")
            }
        }
    }

    private fun startUsbPair() {
        if (usbRunning) return
        val manager = host.getSystemService(UsbManager::class.java)
        if (manager == null) {
            showUsbError("NODEVICE")
            return
        }
        usbManager = manager
        val device = manager.deviceList.values.firstOrNull { it.vendorId == APPLE_VID }
        if (device == null) {
            AppLog.write(filesDir, "USB 配對：找不到 Apple USB 裝置（清單共 ${manager.deviceList.size} 台）")
            showUsbError("NODEVICE")
            return
        }
        AppLog.write(
            filesDir,
            "USB 配對：找到裝置 pid=0x${device.productId.toString(16)}，檢查授權"
        )
        if (manager.hasPermission(device)) {
            openAndPair(device)
        } else {
            usbStatus.text = getString(R.string.pair_usb_perm_asking)
            val filter = IntentFilter(ACTION_USB_PERMISSION)
            if (Build.VERSION.SDK_INT >= 33) {
                host.registerReceiver(
                    usbPermissionReceiver, filter, Context.RECEIVER_NOT_EXPORTED
                )
            } else {
                @Suppress("DEPRECATION")
                host.registerReceiver(usbPermissionReceiver, filter)
            }
            usbReceiverRegistered = true
            val flags = if (Build.VERSION.SDK_INT >= 31) {
                PendingIntent.FLAG_MUTABLE
            } else {
                0
            }
            val permissionIntent = PendingIntent.getBroadcast(
                host, 0,
                Intent(ACTION_USB_PERMISSION).setPackage(host.packageName),
                flags
            )
            manager.requestPermission(device, permissionIntent)
        }
    }

    private fun unregisterUsbReceiver() {
        if (!usbReceiverRegistered) return
        usbReceiverRegistered = false
        try {
            host.unregisterReceiver(usbPermissionReceiver)
        } catch (e: IllegalArgumentException) {
            // Already unregistered; nothing to do.
        }
    }

    private fun openAndPair(device: UsbDevice) {
        val manager = usbManager ?: run {
            showUsbError("NODEVICE")
            return
        }
        // The usbmux interface is vendor-specific (class 0xFF,
        // subclass 0xFE, protocol 2) with exactly two bulk
        // endpoints, usbmuxd's criteria. Strict matches win;
        // higher configuration values win ties (usbmuxd scans
        // configurations in descending order there, too).
        val candidates = mutableListOf<MuxCandidate>()
        for (ci in 0 until device.configurationCount) {
            val config = device.getConfiguration(ci)
            for (ii in 0 until config.interfaceCount) {
                val iface = config.getInterface(ii)
                if (iface.interfaceClass != 0xFF || iface.endpointCount != 2) continue
                var epIn: UsbEndpoint? = null
                var epOut: UsbEndpoint? = null
                for (ei in 0 until iface.endpointCount) {
                    val ep = iface.getEndpoint(ei)
                    if (ep.type != UsbConstants.USB_ENDPOINT_XFER_BULK) continue
                    if (ep.direction == UsbConstants.USB_DIR_IN) epIn = ep else epOut = ep
                }
                val input = epIn
                val output = epOut
                if (input != null && output != null) {
                    candidates += MuxCandidate(
                        config,
                        iface,
                        input,
                        output,
                        iface.interfaceSubclass == 0xFE && iface.interfaceProtocol == 2
                    )
                }
            }
        }
        if (candidates.isEmpty()) {
            AppLog.write(filesDir, "USB 配對：裝置上找不到 usbmux 介面")
            showUsbError("NOIFACE")
            return
        }
        candidates.sortWith(
            compareByDescending<MuxCandidate> { it.strict }
                .thenByDescending { it.configValue }
        )
        val chosen = candidates[0]
        val connection = manager.openDevice(device)
        if (connection == null) {
            AppLog.write(filesDir, "USB 配對：openDevice 失敗")
            showUsbError("USBOPEN")
            return
        }
        if (device.configurationCount > 1) {
            val setOk = try {
                connection.setConfiguration(chosen.config)
            } catch (e: Exception) {
                false
            }
            AppLog.write(filesDir, "USB 配對：setConfiguration(${chosen.configValue}) = $setOk")
        }
        if (!connection.claimInterface(chosen.iface, true)) {
            AppLog.write(filesDir, "USB 配對：claimInterface 失敗")
            try {
                connection.close()
            } catch (e: Exception) {
                // Best effort.
            }
            showUsbError("USBOPEN")
            return
        }
        usbConnection = connection
        usbClaimed = chosen.iface
        val note = "pid=0x${device.productId.toString(16)} cfg=${chosen.configValue} " +
            "iface=${chosen.iface.id} epIn=0x${chosen.endpointIn.address.toString(16)} " +
            "epOut=0x${chosen.endpointOut.address.toString(16)} strict=${chosen.strict}"
        AppLog.write(filesDir, "USB 配對：開啟 mux 連結 $note")
        val link = UsbMuxLink(connection, chosen.endpointIn, chosen.endpointOut, note)
        usbRunning = true
        usbButton.isEnabled = false
        usbStatus.text = getString(R.string.pair_usb_busy)
        AppLog.write(filesDir, "USB 配對：開始（請在 iPhone 上點信任）")
        Thread {
            val result = try {
                signFacade.nativeUsbPair(link, filesDir.absolutePath)
            } catch (e: UnsatisfiedLinkError) {
                "ERR\nUNKNOWN\nRust core missing"
            }
            runOnUiThread { onUsbPairDone(link, result) }
        }.start()
    }

    private fun onUsbPairDone(link: UsbMuxLink, result: String) {
        link.close()
        val claimed = usbClaimed
        val connection = usbConnection
        usbClaimed = null
        usbConnection = null
        if (claimed != null && connection != null) {
            try {
                connection.releaseInterface(claimed)
            } catch (e: Exception) {
                // Best effort.
            }
        }
        if (connection != null) {
            try {
                connection.close()
            } catch (e: Exception) {
                // Best effort.
            }
        }
        usbRunning = false
        usbButton.isEnabled = true
        val parts = result.split("\n", limit = 3)
        when (parts[0]) {
            "OK" -> {
                val udid = parts.getOrElse(1) { "" }
                val name = parts.getOrElse(2) { "" }
                showPairing(udid, name)
                usbStatus.text = getString(R.string.pair_usb_ok)
                Toast.makeText(host, getString(R.string.sign_import_ok), Toast.LENGTH_SHORT)
                    .show()
                AppLog.write(filesDir, "USB 配對：成功（裝置 …${udid.takeLast(6)}）")
            }
            else -> showUsbError(
                parts.getOrElse(1) { "UNKNOWN" },
                parts.getOrElse(2) { "" }
            )
        }
    }

    private fun showUsbError(code: String, message: String = "") {
        usbStatus.text = when (code) {
            "NODEVICE" -> getString(R.string.pair_usb_nodevice)
            "USBPERM" -> getString(R.string.pair_usb_perm)
            "NOIFACE" -> getString(R.string.pair_usb_noiface)
            "USBOPEN" -> getString(R.string.pair_usb_open)
            "DENIED" -> getString(R.string.pair_usb_denied)
            "TRUST_TIMEOUT" -> getString(R.string.pair_usb_timeout)
            "USBLOST" -> getString(R.string.pair_usb_lost)
            "PASSCODE" -> getString(R.string.pair_usb_passcode)
            else -> getString(
                R.string.pair_usb_failed,
                message.ifEmpty { code }
            )
        }
        AppLog.write(filesDir, "USB 配對失敗：$code $message")
        refreshPairingUi()
    }

    companion object {
        private const val REQ_PICK_PAIRING = 4301
        private const val APPLE_VID = 0x05AC
        private const val ACTION_USB_PERMISSION =
            "com.yongshuai.ipaloader.USB_PERMISSION"
    }
}
