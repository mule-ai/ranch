package dev.ranch.android

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import org.json.JSONObject
import kotlin.concurrent.Volatile

/**
 * Notification engine — port of the RN `notifyEvents.ts` +
 * `notifications.ts` logic. Fires local notifications for agent events
 * ONLY while the app is in the background; the on-screen UI covers the
 * foreground case. Dedup rules (mirroring the RN client):
 *  - turn_end: only on a working -> idle edge for a pane we SAW working
 *  - every_message: only append frames (reset=false) past the pane's
 *    last-seen seq; the first frame for a pane primes the baseline
 *  - questions: once per ask_id, until the AgentAskAnswer broadcast
 */
class Notify(private val app: App) {
    private val nm =
        app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    // ---- settings (defaults mirror mobile/lib/notifications.ts) ----
    var turnEnd: Boolean
        get() = app.prefs.getBool("turn_end", true)
        set(v) { app.prefs.setBool("turn_end", v) }
    var everyMessage: Boolean
        get() = app.prefs.getBool("every_message", false)
        set(v) { app.prefs.setBool("every_message", v) }
    var ignoreToolCalls: Boolean
        get() = app.prefs.getBool("ignore_tool_calls", true)
        set(v) { app.prefs.setBool("ignore_tool_calls", v) }
    var questions: Boolean
        get() = app.prefs.getBool("questions", true)
        set(v) { app.prefs.setBool("questions", v) }

    // ---- dedup state (single relay thread, no locking needed) ----
    private val busy = HashMap<String, Boolean>()
    private val lastSeq = HashMap<String, Long>()
    private val primed = HashSet<String>()
    private val notifiedAsks = HashSet<String>()

    // ---- diagnostics (in-memory, session scoped) ----
    @Volatile var framesSeen = 0; private set
    @Volatile var lastFrameAt: Long = 0; private set
    @Volatile var firedTurn = 0; private set
    @Volatile var firedMessage = 0; private set
    @Volatile var firedQuestion = 0; private set
    @Volatile var skippedActive = 0; private set
    @Volatile var skippedOff = 0; private set
    @Volatile var errors = 0; private set

    /** Drop transient state on reconnect; keep notifiedAsks. */
    fun reset() {
        busy.clear()
        lastSeq.clear()
        primed.clear()
    }

    /** Call for EVERY frame received. Never throws. */
    fun onFrame(f: JSONObject, sessionName: String) {
        try {
            val t = f.optString("t")
            when (t) {
                "Meta" -> onMeta(f, sessionName)
                "Chat" -> onChat(f, sessionName)
                "AgentAskRequest" -> {
                    framesSeen++
                    lastFrameAt = System.currentTimeMillis()
                    val askId = f.optString("ask_id")
                    if (askId.isNotEmpty() && notifiedAsks.add(askId)) {
                        val q = f.optString("question").take(110)
                        notify("questions", sessionName, "agent question: $q", sessionName)
                    }
                }
                "AgentAskAnswer" -> f.optString("ask_id").takeIf { it.isNotEmpty() }?.let(notifiedAsks::remove)
                else -> Unit
            }
        } catch (e: Exception) {
            errors++
        }
    }

    private fun onMeta(f: JSONObject, name: String) {
        if (f.optString("kind") != "agent") return
        val pane = f.optString("pane")
        if (pane.isEmpty()) return
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        val status = f.optString("status")
        when {
            status == "working" -> busy[pane] = true
            status == "idle" && busy[pane] == true -> {
                busy[pane] = false
                notify("turn_end", name, "agent finished its turn", name)
            }
        }
    }

    private fun onChat(f: JSONObject, name: String) {
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        val pane = f.optString("pane")
        val msgs = f.optJSONArray("msgs") ?: return
        if (msgs.length() == 0) return
        var maxSeq = 0L
        for (i in 0 until msgs.length()) {
            maxSeq = maxOf(maxSeq, msgs.getJSONObject(i).optLong("seq", 0))
        }
        if (f.optBoolean("reset", false)) {
            lastSeq[pane] = maxSeq
            primed.add(pane)
            return
        }
        if (!primed.contains(pane)) {
            lastSeq[pane] = maxSeq
            primed.add(pane)
            return
        }
        val base = lastSeq[pane] ?: -1L
        val fresh = mutableListOf<JSONObject>()
        for (i in 0 until msgs.length()) {
            val m = msgs.getJSONObject(i)
            if (m.optLong("seq", 0) > base) fresh.add(m)
        }
        if (fresh.isEmpty()) return
        lastSeq[pane] = maxSeq
        for (m in fresh) {
            val role = m.optString("role")
            when {
                role == "assistant" && m.optString("text").trim().isNotEmpty() ->
                    notify("every_message", name, m.optString("text").replace(Regex("\\s+"), " ").take(120), name)
                role == "tool" && !ignoreToolCalls ->
                    notify("every_message", name, "tool: " + m.optString("tool_name", "tool"), name)
            }
        }
    }

    private fun notify(kind: String, title: String, body: String, tag: String): Boolean {
        return try {
        val enabled = when (kind) {
            "turn_end" -> turnEnd
            "every_message" -> everyMessage
            "questions" -> questions
            else -> false
        }
        if (!enabled) { skippedOff++; return false }
        if (app.isForeground) { skippedActive++; return false }
        postLocal(title, body, tag)
        when (kind) {
            "turn_end" -> firedTurn++
            "every_message" -> firedMessage++
            "questions" -> firedQuestion++
        }
        true
    } catch (e: Exception) {
        errors++
        false
    }
    }

    /** Bypasses the foreground/settings gates — for the Settings "test" button. */
    fun testNotification(): Pair<Boolean, String> {
        return try {
        if (!nm.areNotificationsEnabled()) return Pair(false, "notifications disabled on device")
        postLocal("ranch", "test notification — if you see this, notifications work", "test")
        Pair(true, "sent — pull down the notification shade")
    } catch (e: Exception) {
        errors++
        Pair(false, "failed: ${e.message}")
    }
    }

    private fun postLocal(title: String, body: String, tag: String) {
        val openIntent = PendingIntent.getActivity(
            app, 0, Intent(app, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(app, "ranch")
            .setSmallIcon(R.drawable.ic_stat_ranch)
            .setContentTitle(title)
            .setContentText(body)
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_REMINDER)
            .setContentIntent(openIntent)
            .setWhen(System.currentTimeMillis())
            .setDefaults(Notification.DEFAULT_ALL)
        if (Build.VERSION.SDK_INT >= 26) {
            builder.setGroup("ranch")
        }
        nm.notify(tag.hashCode(), builder.build())
    }
}
