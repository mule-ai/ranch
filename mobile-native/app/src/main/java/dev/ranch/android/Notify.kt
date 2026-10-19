package dev.ranch.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import org.json.JSONObject
import kotlin.concurrent.Volatile

/**
 * Notification engine — H6.1: AGENT-SCOPED notifications with actions.
 *
 * Fires local notifications for agent events ONLY while the app is in
 * the background; the on-screen UI covers the foreground case.
 *
 * Kinds (defaults in [NotifyPolicy.KIND_DEFAULTS], the shared policy —
 * the Expo/TS twin is `mobile/lib/notify-policy.ts`):
 *   turn_end     pane working -> idle edge (existing; re-tagged with
 *                the agent name + avatar when the pane is resolvable
 *                to an agent via AgentTalk/AgentTalkOk)
 *   errors       `⚠` assistant chat rows (existing)
 *   questions    ad-hoc AgentAsk with no recognizable card lane
 *   approval     policy_ask card ("Allow this action" / "Deny")
 *   research     research_report card ("Use" / "Discard" / "Ask more")
 *   memory       memory_review card ("Keep" / "Forget")
 *   task_done    a forge_task activity item left the ActivityPush set
 *                (its forge state went terminal — the poller only
 *                lists pending/running tasks, so "gone = done"; a feed
 *                outage that hides a task is indistinguishable and
 *                also fires this — documented v1 limitation)
 *   every_message existing per-message mode (only when turn-end off)
 *
 * Actions: actionable kinds carry up to two buttons whose
 * PendingIntents broadcast to [MonitorService], which answers through
 * the foreground service's Realtime socket:
 *   ask lanes  -> AgentAskAnswer { ask_id, choices: [i] }
 *   task_done  -> "Open" (the agent's conversation) + "Fork"
 *                 (AgentTalk { mode: "fork" } via MainActivity).
 *                 The plan's "stop" (TaskAbort frame) is deferred: the
 *                 frame does not exist yet (H5.3 part 3a deferred it
 *                 too) — v1 ships without a stop button.
 *
 * Per-agent mute/priority: `agent_notify` prefs JSON
 * ({name: {muted, priority high|normal|quiet}}) — managed in
 * [NotifyAgentsActivity]. Mute is NOTIFICATION-ONLY: the agent's items
 * still appear in the Activity View. quiet/high select the
 * "ranch.quiet" / "ranch.high" channels.
 *
 * Avatars: agent registry avatar_url downloaded to
 * cacheDir/avatars (see [Avatars]), shown via setLargeIcon +
 * BigPictureStyle (custom bitmaps are not allowed as small icons on
 * modern Android — the app icon is the small icon).
 *
 * Dedup rules (mirroring the RN client):
 *  - turn_end: only on a working -> idle edge for a pane we SAW working
 *  - every_message: only append frames (reset=false) past the pane's
 *    last-seen seq; the first frame for a pane primes the baseline,
 *    and every frame within QUIET_WINDOW_MS of priming is folded into
 *    the baseline silently — a forge history replay (watch armed with
 *    since=0: daemon cold start, relay reconnect re-watch) lands as
 *    one Chat frame per backlog row, and per-frame priming alone
 *    would notify the WHOLE backlog (recurring notification bursts on
 *    every EA chat open, 2026-10-19)
 *  - questions/cards: once per ask_id, until the AgentAskAnswer
 *    broadcast
 */
class Notify(private val app: App) {
    private val nm =
        app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    init {
        // per-priority channels ("ranch" is created at app start)
        nm.createNotificationChannel(
            NotificationChannel("ranch.quiet", "ranch (quiet)", NotificationManager.IMPORTANCE_LOW)
        )
        nm.createNotificationChannel(
            NotificationChannel("ranch.high", "ranch (high)", NotificationManager.IMPORTANCE_HIGH)
        )
    }

    // ---- global settings (defaults mirror mobile/lib/notify-policy.ts) ----
    var turnEnd: Boolean
        get() = app.prefs.getBool("turn_end", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.TURN_END]!!)
        set(v) { app.prefs.setBool("turn_end", v) }
    var everyMessage: Boolean
        get() = app.prefs.getBool("every_message", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.EVERY_MESSAGE]!!)
        set(v) { app.prefs.setBool("every_message", v) }
    var ignoreToolCalls: Boolean
        get() = app.prefs.getBool("ignore_tool_calls", true)
        set(v) { app.prefs.setBool("ignore_tool_calls", v) }
    var questions: Boolean
        get() = app.prefs.getBool("questions", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.QUESTIONS]!!)
        set(v) { app.prefs.setBool("questions", v) }
    var notifyErrors: Boolean
        get() = app.prefs.getBool("errors", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.ERRORS]!!)
        set(v) { app.prefs.setBool("errors", v) }
    var approval: Boolean
        get() = app.prefs.getBool("approval", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.APPROVAL]!!)
        set(v) { app.prefs.setBool("approval", v) }
    var research: Boolean
        get() = app.prefs.getBool("research", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.RESEARCH]!!)
        set(v) { app.prefs.setBool("research", v) }
    var memory: Boolean
        get() = app.prefs.getBool("memory", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.MEMORY]!!)
        set(v) { app.prefs.setBool("memory", v) }
    var taskDone: Boolean
        get() = app.prefs.getBool("task_done", NotifyPolicy.KIND_DEFAULTS[NotifyPolicy.TASK_DONE]!!)
        set(v) { app.prefs.setBool("task_done", v) }

    private fun kindEnabled(kind: String): Boolean = when (kind) {
        NotifyPolicy.TURN_END -> turnEnd
        NotifyPolicy.EVERY_MESSAGE -> everyMessage
        NotifyPolicy.QUESTIONS -> questions
        NotifyPolicy.ERRORS -> notifyErrors
        NotifyPolicy.APPROVAL -> approval
        NotifyPolicy.RESEARCH -> research
        NotifyPolicy.MEMORY -> memory
        NotifyPolicy.TASK_DONE -> taskDone
        else -> false
    }

    /** Per-agent settings; unknown/absent agents get the defaults. */
    private fun agentPrefs(name: String?): NotifyPolicy.AgentPrefs =
        name?.let { Avatars.parseAgentPrefs(app.prefs.getJson("agent_notify"))[it] }
            ?: NotifyPolicy.AgentPrefs()

    // ---- dedup state (single relay thread, no locking needed) ----
    private val busy = HashMap<String, Boolean>()
    private val lastSeq = HashMap<String, Long>()
    private val primed = HashSet<String>()
    // pane -> ms epoch at which it was primed; frames inside the quiet
    // window after priming are a HISTORY REPLAY burst (one frame per
    // backlog row) and fold into the baseline without notifying.
    private val quietStart = HashMap<String, Long>()
    private val notifiedAsks = HashSet<String>()
    // pane -> ranch session id, captured from any frame carrying both —
    // notification taps deep-link into the conversation that fired them
    private val paneSession = HashMap<String, String>()
    // last assistant message of the current turn, per pane — the
    // turn-end notification carries it so the ping lands on the LAST
    // message (with the actual reply), not the first
    private val turnLast = HashMap<String, String>()
    // H6.1 task_done: forge_task item ids currently in the ActivityPush
    // set; an id leaving the set = its forge task went terminal
    private val seenTasks = HashSet<String>()
    private var tasksPrimed = false

    // F-006 (2026-10-19): keys of AgentNotify frames already posted —
    // survives reset() AND process restart (persisted below) so a
    // replay of the @notify: backlog can never re-ping. Bounded FIFO
    // via markAgentNotified().
    private val notifiedAgentMsgs = LinkedHashSet<String>()

    init {
        // re-arm the dedup set across process restarts (the whole
        // point is surviving the app-relaunch-replays-backlog case).
        runCatching {
            val raw = app.prefs.getJson("agent_notify_seen")
            if (raw.isNotEmpty()) {
                val arr = org.json.JSONArray(raw)
                for (i in 0 until arr.length()) notifiedAgentMsgs.add(arr.getString(i))
            }
        }
    }

    // ---- diagnostics (in-memory, session scoped) ----
    @Volatile var framesSeen = 0; private set
    @Volatile var lastFrameAt: Long = 0; private set
    @Volatile var firedTurn = 0; private set
    @Volatile var firedMessage = 0; private set
    @Volatile var firedQuestion = 0; private set
    @Volatile var firedError = 0; private set
    @Volatile var firedCard = 0; private set
    @Volatile var firedTask = 0; private set
    @Volatile var skippedActive = 0; private set
    @Volatile var skippedOff = 0; private set
    @Volatile var errors = 0; private set

    /** Drop transient state on reconnect; keep notifiedAsks. */
    fun reset() {
        busy.clear()
        lastSeq.clear()
        primed.clear()
        quietStart.clear()
        turnLast.clear()
        paneSession.clear()
        seenTasks.clear()
        tasksPrimed = false
    }

    /** Call for EVERY frame received. Never throws. */
    fun onFrame(f: JSONObject, sessionName: String) {
        try {
            val t = f.optString("t")
            when (t) {
                "Meta" -> onMeta(f, sessionName)
                "Chat" -> onChat(f, sessionName)
                "AgentAskRequest" -> onAsk(f, sessionName)
                "AgentAskAnswer" -> f.optString("ask_id").takeIf { it.isNotEmpty() }?.let(notifiedAsks::remove)
                "ActivityPush" -> onActivityPush(f)
                "AgentNotify" -> onAgentNotify(f)
                else -> Unit
            }
        } catch (e: Exception) {
            errors++
        }
    }

    /** Remember pane -> session from any frame that carries both. */
    private fun rememberSession(f: JSONObject) {
        val pane = f.optString("pane")
        val sid = f.optString("session")
        if (pane.isNotEmpty() && sid.isNotEmpty()) paneSession[pane] = sid
    }

    private fun onMeta(f: JSONObject, name: String) {
        if (f.optString("kind") != "agent") return
        val pane = f.optString("pane")
        if (pane.isEmpty()) return
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        rememberSession(f)
        val status = f.optString("status")
        val background = f.optBoolean("background", false)
        when {
            status == "working" -> {
                busy[pane] = true
                turnLast.remove(pane)
            }
            status == "idle" && busy[pane] == true -> {
                busy[pane] = false
                if (background) {
                    // background turn (mule wake / cron) — suppress
                    // the turn-end notification; the agent can still
                    // fire an explicit AgentNotify if it matters
                    turnLast.remove(pane)
                    skippedOff++
                    return
                }
                val agent = Monitor.agentFor(pane, f.optString("session"))
                val body = turnLast.remove(pane)
                    ?.replace(Regex("\\s+"), " ")
                    ?.take(160)
                    ?: "agent finished its turn"
                notify(NotifyPolicy.TURN_END, agent ?: name, body, agent, pane)
            }
        }
    }

    private fun onChat(f: JSONObject, name: String) {
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        rememberSession(f)
        val pane = f.optString("pane")
        val agent = Monitor.agentFor(pane, f.optString("session"))
        val msgs = f.optJSONArray("msgs") ?: return
        if (msgs.length() == 0) return
        var maxSeq = 0L
        for (i in 0 until msgs.length()) {
            maxSeq = maxOf(maxSeq, msgs.getJSONObject(i).optLong("seq", 0))
        }
        if (f.optBoolean("reset", false)) {
            lastSeq[pane] = maxSeq
            primed.add(pane)
            quietStart.remove(pane)
            return
        }
        if (!primed.contains(pane)) {
            lastSeq[pane] = maxSeq
            primed.add(pane)
            quietStart[pane] = System.currentTimeMillis()
            return
        }
        // Quiet window: the backlog replay that follows a prime
        // arrives as a rapid per-row frame stream (forge SSE catch-up).
        // Fold everything inside the window into the baseline —
        // notifying a whole history every time a watch replays is the
        // bug this guard kills. Live human-paced rows never fall in
        // the window except in the first instants after a (re)watch,
        // where the turn-end Meta still notifies the reply.
        val qs = quietStart[pane]
        if (qs != null && System.currentTimeMillis() - qs < QUIET_WINDOW_MS) {
            lastSeq[pane] = maxOf(lastSeq[pane] ?: 0L, maxSeq)
            skippedOff++
            return
        }
        quietStart.remove(pane)
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
            val text = m.optString("text")
            when {
                role == "assistant" && text.trim().isNotEmpty() && text.trim().startsWith("⚠") -> {
                    // agent error rows (failed pi turns, exhausted retries,
                    // failed sends) get their own default-on notification —
                    // and suppress the follow-up "agent finished its turn",
                    // because the error IS the turn-end news
                    busy[pane] = false
                    turnLast.remove(pane)
                    notify(
                        NotifyPolicy.ERRORS, agent ?: name,
                        "agent error: " + text.replace(Regex("\\s+"), " ").take(120), agent, pane,
                    )
                }
                role == "assistant" && text.trim().isNotEmpty() -> {
                    if (turnEnd) {
                        // batch into the turn-end notification: pinging per
                        // row means the FIRST message wakes the user, they
                        // open the app, and the final reply never notifies
                        turnLast[pane] = text
                    } else {
                        notify(
                            NotifyPolicy.EVERY_MESSAGE, agent ?: name,
                            text.replace(Regex("\\s+"), " ").take(120), agent, pane,
                        )
                    }
                }
                role == "tool" && !ignoreToolCalls && !turnEnd ->
                    notify(
                        NotifyPolicy.EVERY_MESSAGE, agent ?: name,
                        "tool: " + m.optString("tool_name", "tool"), agent, pane,
                    )
            }
        }
    }

    /** H6.1: an AgentAskRequest — classify the card lane and post an
     *  agent-scoped, ACTIONABLE notification (choice buttons). */
    private fun onAsk(f: JSONObject, name: String) {
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        rememberSession(f)
        val askId = f.optString("ask_id")
        if (askId.isEmpty() || !notifiedAsks.add(askId)) return
        val choices = mutableListOf<String>()
        f.optJSONArray("choices")?.let { arr ->
            for (i in 0 until arr.length()) choices.add(arr.getString(i))
        }
        val lane = NotifyPolicy.laneForChoices(choices)
        val pane = f.optString("pane")
        val session = f.optString("session")
        val agent = Monitor.agentFor(pane, session)
        val q = f.optString("question").replace(Regex("\\s+"), " ").take(110)
        val body = when (lane) {
            NotifyPolicy.QUESTIONS -> "agent question: $q"
            else -> "${lane}: $q"
        }
        val actions = NotifyPolicy.actionIndices(choices).map {
            NotifyAction(NotifyPolicy.actionLabel(choices[it]), askAction(askId, it))
        }
        val fired = notify(lane, agent ?: name, body, agent, pane, session, actions)
        if (!fired) notifiedAsks.remove(askId)
        // ask re-broadcasts (attach/reconnect) re-arm the notification
        // so a question the user never saw can still ping later
    }

    /** H6.1 task_done source: the ActivityPush set diffing. The daemon
     *  poller only lists NON-TERMINAL forge tasks, so a previously-seen
     *  forge_task item leaving the set means its state went terminal
     *  (done/failed/discarded — not distinguishable from the item). */
    private fun onActivityPush(f: JSONObject) {
        val items = f.optJSONArray("items") ?: return
        val now = HashSet<String>()
        for (i in 0 until items.length()) {
            val it = items.getJSONObject(i)
            if (it.optString("kind") != "forge_task") continue
            val id = it.optString("id")
            if (id.isNotEmpty() && it.optString("agent").isNotEmpty()) now.add(id)
        }
        if (!tasksPrimed) {
            tasksPrimed = true
            seenTasks.clear()
            seenTasks.addAll(now)
            return
        }
        for (key in seenTasks) {
            if (now.contains(key)) continue
            // item id shape: "forge_task/<agentName>/<taskId>"
            val rest = key.removePrefix("forge_task/")
            val agent = rest.substringBeforeLast('/', "")
            if (agent.isEmpty()) continue
            val actions = listOf(
                NotifyAction("Open", taskOpenIntent(agent, "continue")),
                NotifyAction("Fork", taskOpenIntent(agent, "fork")),
            )
            notify(
                NotifyPolicy.TASK_DONE, agent, "background task finished", agent, null, null, actions,
            )
        }
        seenTasks.clear()
        seenTasks.addAll(now)
    }

    /** Explicit agent notification (the agent called notify_user).
     *  Always posts — this is the opt-in path for background turns.
     *
     *  Dedup (mobile bug 2026-10-19, F-006): the frame used to carry
     *  no identity at all, so ANY re-delivery — SSE catch-up replay
     *  of the @notify: row, a second pane watching the same session,
     *  an app restart inside a replay window — re-posted the ping,
     *  and opening the conversation re-notified the whole backlog.
     *  The daemon now stamps the forge row `seq` (protocol
     *  2026-10-19); we key on (session|pane, seq) and fold repeats
     *  silently. With an old daemon (no seq) fall back to a content
     *  hash — weaker (two identical messages legitimately sent hours
     *  apart collapse) but strictly better than pinging the backlog
     *  on every open. The key set persists to prefs so a process
     *  restart doesn't re-arm the backlog either. */
    private fun onAgentNotify(f: JSONObject) {
        framesSeen++
        lastFrameAt = System.currentTimeMillis()
        val agent = f.optString("agent").ifEmpty { "agent" }
        val msg = f.optString("message").replace(Regex("\\s+"), " ").take(200)
        val priority = f.optString("priority").ifEmpty { "normal" }
        val pane = f.optString("pane").takeIf { it.isNotEmpty() }
        val key = agentNotifyKey(f, msg)
        if (key != null && !markAgentNotified(key)) {
            skippedOff++
            return // replay/re-delivery — already posted
        }
        // Bypass the background gate: this is an explicit notification
        postLocal(
            agent, msg,
            agent, pane,
            emptyList(), priority,
        )
    }

    /** Dedup key: prefer (session|pane, seq) — stable across panes
     *  that watch the same session (the daemon fills `session` before
     *  broadcast). Content-hash fallback when the daemon predates seq.
     *  Returns null only when there is nothing to key on at all
     *  (unattributed frame) — post it rather than lose a real ping. */
    private fun agentNotifyKey(f: JSONObject, msg: String): String? {
        val session = f.optString("session").ifEmpty { f.optString("pane") }
        val seq = if (f.has("seq")) f.optLong("seq", -1) else -1
        if (session.isEmpty() && seq < 0) return null
        val ident = if (seq >= 0) "s$seq" else "h" + msg.hashCode()
        return "$session|$ident"
    }

    /** Returns true when the key is NEW (post it); false when already
     *  seen (suppress). Bounded FIFO so the persisted set can't grow
     *  without limit; persists on every new key (cheap — notify_user
     *  is rare by design). */
    private fun markAgentNotified(key: String): Boolean {
        if (!notifiedAgentMsgs.add(key)) return false
        while (notifiedAgentMsgs.size > AGENT_NOTIFY_DEDUP_MAX) {
            val it = notifiedAgentMsgs.iterator(); it.next(); it.remove()
        }
        persistAgentNotifyKeys()
        return true
    }

    private fun persistAgentNotifyKeys() {
        runCatching {
            val arr = org.json.JSONArray()
            notifiedAgentMsgs.forEach { arr.put(it) }
            app.prefs.setJson("agent_notify_seen", arr.toString())
        }
    }

    // ---- gating + posting ----

    private fun notify(
        kind: String,
        title: String,
        body: String,
        agent: String? = null,
        pane: String? = null,
        session: String? = null,
        actions: List<NotifyAction> = emptyList(),
    ): Boolean {
        return try {
            val prefs = agentPrefs(agent)
            if (!NotifyPolicy.decision(kindEnabled(kind), prefs)) {
                skippedOff++
                return false
            }
            if (app.isForeground) {
                // an actionable question/card is actionable no matter where
                // the user is in the app — suppress only when THAT
                // conversation is the one on screen (the card is right there)
                val onScreen = session != null && app.foregroundSession == session
                if (!(NotifyPolicy.ACTIONABLE.contains(kind) && !onScreen)) {
                    skippedActive++
                    return false
                }
            }
            postLocal(title, body, agent, pane, actions, prefs.priority)
            when (kind) {
                NotifyPolicy.TURN_END -> firedTurn++
                NotifyPolicy.EVERY_MESSAGE -> firedMessage++
                NotifyPolicy.QUESTIONS -> firedQuestion++
                NotifyPolicy.ERRORS -> firedError++
                NotifyPolicy.TASK_DONE -> firedTask++
                else -> firedCard++
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
            postLocal("ranch", "test notification — if you see this, notifications work", null, null, emptyList(), "normal")
            Pair(true, "sent — pull down the notification shade")
        } catch (e: Exception) {
            errors++
            Pair(false, "failed: ${e.message}")
        }
    }

    private fun channelFor(priority: String): String = when (priority) {
        "high" -> "ranch.high"
        "quiet" -> "ranch.quiet"
        else -> "ranch"
    }

    /**
     * One notification per pane (tag = session id, or the legacy tag).
     * Tapping deep-links into the conversation that fired it; buttons
     * route through [MonitorService] (see [onAsk]).
     */
    private fun postLocal(
        title: String,
        body: String,
        agent: String?,
        pane: String?,
        actions: List<NotifyAction>,
        priority: String,
    ) {
        // deep link: tapping opens the conversation that fired this —
        // MainActivity consumes the extras once the monitor is up
        val intent = Intent(app, MainActivity::class.java)
        val sid = pane?.let { paneSession[it] }
        // the session name beats the machine name as title; the agent
        // name beats both (H6.1); each session gets its own shade entry
        val sname = sid?.let { s ->
            Monitor.sessions.firstOrNull { it.id == s }?.name
                .takeUnless { it.isNullOrEmpty() }
        }
        val finalTitle = agent?.takeUnless { it.isEmpty() } ?: sname ?: title
        var notifTag = title
        if (!sid.isNullOrEmpty()) {
            intent.putExtra("open_session", sid)
            intent.putExtra("open_session_name", sname ?: finalTitle)
            if (!pane.isNullOrEmpty()) intent.putExtra("open_pane", pane)
            notifTag = sid
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val openIntent = PendingIntent.getActivity(
            app, notifTag.hashCode(), intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        var builder = Notification.Builder(app, channelFor(priority))
            .setSmallIcon(R.drawable.ic_stat_ranch)
            .setContentTitle(finalTitle)
            .setContentText(body)
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_REMINDER)
            .setContentIntent(openIntent)
            .setWhen(System.currentTimeMillis())
            .setDefaults(Notification.DEFAULT_ALL)
        if (priority == "high") {
            @Suppress("DEPRECATION")
            builder.setPriority(Notification.PRIORITY_HIGH)
        }
        // H6.1 avatar: big picture + large icon (custom bitmaps are not
        // allowed as the small icon on modern Android — the app icon is)
        val url = agent?.let { Monitor.agentAvatarUrl(it) }
        if (!url.isNullOrEmpty()) {
            val bmp = Avatars.bitmapFor(url)
            if (bmp != null) {
                builder.setLargeIcon(bmp)
                builder.setStyle(Notification.BigPictureStyle().bigPicture(bmp))
            } else {
                Avatars.ensure(url) // next notification gets it
            }
        }
        for (a in actions) {
            builder.addAction(0, a.label, a.intent)
        }
        nm.notify(notifTag.hashCode(), builder.build())
    }

    /** Button -> MonitorService -> Realtime `AgentAskAnswer`. */
    private fun askAction(askId: String, choiceIndex: Int): PendingIntent {
        val i = Intent(ACTION_ASK)
            .setPackage(app.packageName)
            .putExtra("ask_id", askId)
            .putExtra("choice", choiceIndex)
        return PendingIntent.getService(
            app, "ask:$askId:$choiceIndex".hashCode(), i,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    /** "Open"/"Fork" on a task notification: launch the app; MainActivity
     *  resolves the agent + sends `AgentTalk` and opens the pane. */
    private fun taskOpenIntent(agent: String, mode: String): PendingIntent {
        val i = Intent(app, MainActivity::class.java)
            .putExtra("open_agent", agent)
            .putExtra("open_agent_mode", mode)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        return PendingIntent.getActivity(
            app, "task:$agent:$mode".hashCode(), i,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    companion object {
        /** cap for the persisted AgentNotify dedup FIFO (F-006) */
        const val AGENT_NOTIFY_DEDUP_MAX = 500
        /** MonitorService intent action for notification buttons. */
        const val ACTION_ASK = "dev.ranch.android.NOTIFY_ASK"
        /** Post-prime silence for history-replay bursts (see onChat). */
        const val QUIET_WINDOW_MS = 1_500L
    }
}

/** One notification button: label + the PendingIntent it fires. */
data class NotifyAction(val label: String, val intent: PendingIntent)
