package com.yongshuai.ipaloader

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder

/**
 * Foreground service hosting the local relays: the [WispServer]
 * WISP pipe (v1) and the [ProxyServer] executing proxy (v2). While
 * it runs, the site's WISP URL can point at this phone
 * (ws://127.0.0.1:…) and the login traffic leaves through the
 * phone's own connection. Started/stopped only from the UI toggle
 * or the notification's stop action; the persistent notification is
 * the "轉發中" signal.
 */
class WispRelayService : Service() {

    enum class State { STOPPED, RUNNING, FAILED }

    companion object {
        private const val ACTION_STOP = "com.yongshuai.ipaloader.wisp.STOP"
        private const val CHANNEL_ID = "wisp_relay"
        private const val NOTIFICATION_ID = 1001

        @Volatile
        var state: State = State.STOPPED
            private set

        @Volatile
        var failureMessage: String? = null
            private set

        fun start(context: Context) {
            context.startForegroundService(Intent(context, WispRelayService::class.java))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, WispRelayService::class.java))
        }
    }

    private var server: WispServer? = null
    private var proxyServer: ProxyServer? = null

    override fun onCreate() {
        super.onCreate()
        val manager = getSystemService(NotificationManager::class.java)
        manager?.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.wisp_notif_channel),
                NotificationManager.IMPORTANCE_LOW,
            )
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // startForegroundService requires startForeground within
        // ~5 s; do it unconditionally before anything else.
        startForeground(NOTIFICATION_ID, buildNotification())
        if (intent?.action == ACTION_STOP) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        if (server == null) {
            failureMessage = null
            val srv = WispServer(
                port = WispStore.PORT,
                expectedPath = { WispStore.expectedPath(applicationContext) },
                log = { message -> AppLog.write(filesDir, message) },
                onStarted = { state = State.RUNNING },
                onFailed = { message ->
                    failureMessage = message
                    state = State.FAILED
                    stopSelf()
                },
            )
            server = srv
            try {
                srv.start()
            } catch (e: Exception) {
                failureMessage = e.message
                state = State.FAILED
                stopSelf()
            }
        }
        if (proxyServer == null) {
            // The v2 proxy shares this service's lifecycle: the same
            // toggle runs both relays. Its bind is synchronous; a
            // failure is logged but does not take the WISP pipe down.
            val proxy = ProxyServer(
                port = WispStore.PROXY_PORT,
                expectedToken = { WispStore.secret(applicationContext) },
                filesDirPath = filesDir.absolutePath,
                log = { message -> AppLog.write(filesDir, message) },
            )
            proxyServer = proxy
            try {
                proxy.start()
            } catch (e: Exception) {
                AppLog.write(filesDir, "本機代理啟動失敗：${e.message}")
                proxyServer = null
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        try {
            proxyServer?.stop()
        } catch (e: Exception) {
            // Already stopped.
        }
        proxyServer = null
        try {
            server?.stop(1000)
        } catch (e: Exception) {
            // Already stopped.
        }
        server = null
        state = State.STOPPED
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun buildNotification(): Notification {
        val openIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this,
            1,
            Intent(this, WispRelayService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setContentTitle(getString(R.string.wisp_notif_title))
            .setContentText(getString(R.string.wisp_notif_text))
            .setOngoing(true)
            .setContentIntent(openIntent)
            .addAction(0, getString(R.string.wisp_stop), stopIntent)
            .build()
    }
}
