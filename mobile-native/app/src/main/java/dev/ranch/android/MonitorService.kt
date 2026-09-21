package dev.ranch.android

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import android.os.Build

/**
 * Foreground service that keeps the Supabase Realtime WebSocket alive
 * while the app is in the background. The service shows a persistent
 * low-importance notification ("Ranch: monitoring …") so Android does
 * not kill the process.
 */
class MonitorService : Service() {

    override fun onBind(intent: Intent): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == "stop") {
            Monitor.stop()
            stopForegroundCompat()
            stopSelf()
            return START_NOT_STICKY
        }

        startForegroundCompat()

        // Restore the machine from the intent, or from Prefs when Android
        // re-starts us after a process kill (START_STICKY, intent may be null).
        val app = this.applicationContext as App
        val mid = intent?.getStringExtra("machineId")
            ?: app.prefs.get("machine_id", "")
        val mname = intent?.getStringExtra("machineName")
            ?: app.prefs.get("machine_name", "")
        if (mid.isNotEmpty() && !Monitor.running) {
            Monitor.start(app, mid, mname.ifEmpty { mid })
        }

        return START_STICKY
    }

    private fun startForegroundCompat() {
        val openIntent = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notif = Notification.Builder(this, "ranch.service")
            .setSmallIcon(R.drawable.ic_stat_ranch)
            .setContentTitle("Ranch")
            .setContentText(if (Monitor.machineName.isNotEmpty()) "monitoring ${Monitor.machineName}" else "monitoring")
            .setContentIntent(openIntent)
            .setOngoing(true)
            .setShowWhen(false)
            .build()
        if (Build.VERSION.SDK_INT >= 31) {
            startForeground(
                SVC_ID,
                notif,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
            )
        } else {
            startForeground(SVC_ID, notif)
        }
    }

    private fun stopForegroundCompat() {
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    companion object {
        const val SVC_ID = 42
    }
}
