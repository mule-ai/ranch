package dev.ranch.android

import android.app.Activity
import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Bundle

/**
 * Application: creates notification channels and tracks whether any
 * activity is in the foreground (so agent-event notifications only fire
 * when the user is NOT looking at the app — same gate as the RN client).
 */
class App : Application() {
    lateinit var prefs: Prefs

    @Volatile
    private var fgCount = 0
    val isForeground: Boolean get() = fgCount > 0

    override fun onCreate() {
        super.onCreate()
        prefs = Prefs(this)
        // crash log: last uncaught exception lands in filesDir/crash.txt so
        // on-device crashes are debuggable without adb
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            try {
                java.io.File(filesDir, "crash.txt").writeText(
                    java.text.SimpleDateFormat("yyyy-MM-dd HH:mm:ss", java.util.Locale.US)
                        .format(java.util.Date()) + " thread=" + t.name + "\n" +
                    e.stackTraceToString()
                )
            } catch (_: Exception) {}
            android.os.Process.killProcess(android.os.Process.myPid())
        }
        val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel("ranch", "ranch", NotificationManager.IMPORTANCE_HIGH)
        )
        nm.createNotificationChannel(
            NotificationChannel("ranch.service", "ranch service", NotificationManager.IMPORTANCE_LOW)
        )
        registerActivityLifecycleCallbacks(object : ActivityLifecycleCallbacks {
            override fun onActivityCreated(a: Activity, b: Bundle?) {}
            override fun onActivityStarted(a: Activity) {}
            override fun onActivityResumed(a: Activity) {
                synchronized(this@App) { fgCount++ }
            }
            override fun onActivityPaused(a: Activity) {
                synchronized(this@App) { fgCount-- }
            }
            override fun onActivityStopped(a: Activity) {}
            override fun onActivitySaveInstanceState(a: Activity, b: Bundle) {}
            override fun onActivityDestroyed(a: Activity) {}
        })
    }
}
