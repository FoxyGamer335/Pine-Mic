package dev.pinemic

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager

/** Foreground service (type "microphone") so streaming survives the screen turning off. */
class MicService : Service() {

    companion object {
        const val ACTION_START = "dev.pinemic.START"
        const val ACTION_STOP = "dev.pinemic.STOP"
        private const val CHANNEL_ID = "pinemic_stream"
        private const val NOTIF_ID = 1

        @Volatile var running = false
        @Volatile var status = "Stopped"
        @Volatile var statusListener: ((String) -> Unit)? = null
        @Volatile var activeStreamer: Streamer? = null

        fun restartCapture() {
            activeStreamer?.requestCaptureRestart()
        }

        private fun publish(s: String) {
            status = s
            statusListener?.invoke(s)
        }
    }

    private var streamer: Streamer? = null
        private var wakeLock: PowerManager.WakeLock? = null
            private var wifiLock: WifiManager.WifiLock? = null

                override fun onBind(intent: Intent?): IBinder? = null

                override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
                    if (intent?.action == ACTION_STOP) {
                        stopStreaming()
                        stopSelf()
                        return START_NOT_STICKY
                    }
                    if (streamer == null) startStreaming()
                        return START_NOT_STICKY
                }

                private fun startStreaming() {
                    createChannel()
                    startForeground(NOTIF_ID, buildNotification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)

                    val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
                    wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "pinemic:stream").apply { acquire() }

                    val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
                    wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "pinemic:wifi").apply { acquire() }

                    running = true
                    streamer = Streamer(applicationContext) { publish(it) }.also {
                        activeStreamer = it
                        it.start()
                    }
                }

                private fun stopStreaming() {
                    activeStreamer = null
                    streamer?.stop()
                    streamer = null
                    try { wakeLock?.takeIf { it.isHeld }?.release() } catch (_: Exception) {}
                    try { wifiLock?.takeIf { it.isHeld }?.release() } catch (_: Exception) {}
                    wakeLock = null
                    wifiLock = null
                    if (running) {
                        running = false
                        publish("Stopped")
                        stopForeground(STOP_FOREGROUND_REMOVE)
                    }
                }

                override fun onDestroy() {
                    stopStreaming()
                    super.onDestroy()
                }

                private fun createChannel() {
                    val nm = getSystemService(NotificationManager::class.java)
                    nm.createNotificationChannel(
                        NotificationChannel(CHANNEL_ID, "Microphone streaming", NotificationManager.IMPORTANCE_LOW)
                    )
                }

                private fun buildNotification(): Notification {
                    val stopIntent = PendingIntent.getService(
                        this, 0,
                        Intent(this, MicService::class.java).setAction(ACTION_STOP),
                                                              PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                    )
                    val openIntent = PendingIntent.getActivity(
                        this, 1, Intent(this, MainActivity::class.java),
                                                               PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                    )
                    val stopAction = Notification.Action.Builder(
                        Icon.createWithResource(this, android.R.drawable.ic_media_pause), "Stop", stopIntent
                    ).build()

                    return Notification.Builder(this, CHANNEL_ID)
                    .setContentTitle("Pine Mic is active")
                    .setContentText("Your microphone is available to the Linux app")
                    .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                    .setContentIntent(openIntent)
                    .setOngoing(true)
                    .addAction(stopAction)
                    .build()
                }
}
