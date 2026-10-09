package dev.ranch.android

import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/**
 * Singleton that tracks the active monitoring session and exposes
 * diagnostics + a frame bus to the UI without holding a service reference.
 *
 * The Realtime socket lives in the foreground service. Both the Notify
 * engine (background notifications) and the active session/terminal screen
 * register as frame sinks on the same channel.
 */
object Monitor {
    var status: String = "stopped"
    var machineId: String = ""
        private set
    var machineName: String = ""
        private set
    @Volatile
    var running: Boolean = false
        private set

    @Volatile
    private var session: RelaySession? = null

    /** Live session list (from HelloOk + SessionsAck + Meta exited). */
    @Volatile
    var sessions: List<Term.SessionMeta> = emptyList()
        private set

    /** Pending agent questions: ask_id -> (session id, question text).
     *  Feeds the home-screen badge so an unanswered ask is visible even
     *  when its session isn't open. */
    val pendingAsks = java.util.concurrent.ConcurrentHashMap<String, Pair<String, String>>()

    /** Version string the daemon reported in HelloOk ("" = unknown). */
    @Volatile
    var daemonVersion: String = ""

    // ---- H6.1 agent identity (feeds the notification engine) ----
    // id -> registry name / avatar url, from AgentListOk (any client's
    // request refreshes this; Monitor.start also warms it)
    val agentNames = java.util.concurrent.ConcurrentHashMap<String, String>()
    val agentAvatars = java.util.concurrent.ConcurrentHashMap<String, String>() // id -> url
    // pane / session -> registry name, from AgentTalk + AgentTalkOk
    val paneAgent = java.util.concurrent.ConcurrentHashMap<String, String>()
    val sessionAgent = java.util.concurrent.ConcurrentHashMap<String, String>()

    /** Best-effort agent name for a pane (or its session). */
    fun agentFor(pane: String?, session: String?): String? =
        pane?.takeIf { it.isNotEmpty() }?.let { paneAgent[it] }
            ?: session?.takeIf { it.isNotEmpty() }?.let { sessionAgent[it] }

    fun agentIdFor(name: String): String? =
        agentNames.entries.firstOrNull { it.value == name }?.key

    fun agentAvatarUrl(name: String): String? =
        agentIdFor(name)?.let { agentAvatars[it] }

    /** Notification button tapped while the app is backgrounded:
     *  answer the ask through the (possibly just-started) relay —
     *  sendFrame queues until the channel is joined. */
    fun handleAskAction(askId: String, choice: Int) {
        if (askId.isEmpty() || choice < 0) return
        relay?.send(Term.agentAskAnswer(askId, listOf(choice), ""))
    }

    fun publishSessions(s: List<Term.SessionMeta>) { sessions = s }

    fun start(app: App, machineId: String, machineName: String) {
        stop()
        this.machineId = machineId
        this.machineName = machineName
        sessions = emptyList()
        pendingAsks.clear()
        daemonVersion = ""
        agentNames.clear()
        agentAvatars.clear()
        paneAgent.clear()
        sessionAgent.clear()
        val s = RelaySession(app, machineId)
        Avatars.init(app)
        session = s
        running = true
        s.start()
        s.send(Term.agentList()) // warm the registry + avatar prefetch
    }

    fun stop() {
        session?.stop()
        session = null
        running = false
        status = "stopped"
        sessions = emptyList()
        pendingAsks.clear()
        daemonVersion = ""
        agentNames.clear()
        agentAvatars.clear()
        paneAgent.clear()
        sessionAgent.clear()
    }

    val relay: RelaySession? get() = session

    /** Diagnostics snapshot for the UI. */
    fun diag(): Map<String, Any> {
        val n = session?.notify
        return mapOf(
            "status" to status,
            "machine" to machineName,
            "frames" to (n?.framesSeen ?: 0),
            "firedTurn" to (n?.firedTurn ?: 0),
            "firedMsg" to (n?.firedMessage ?: 0),
            "firedQ" to (n?.firedQuestion ?: 0),
            "firedError" to (n?.firedError ?: 0),
            "skipActive" to (n?.skippedActive ?: 0),
            "skipOff" to (n?.skippedOff ?: 0),
            "errors" to (n?.errors ?: 0),
            "sessions" to sessions.size,
        )
    }
}

/**
 * A single monitoring session: one machine, one Realtime channel.
 * Frames are fanned out to every registered sink; the Notify engine is
 * always registered, and the active terminal/screen registers itself too.
 */
class RelaySession(
    private val app: App,
    machineId: String,
) {
    val auth: Auth = Auth(app.prefs)
    val notify: Notify = Notify(app)

    private val sinks = CopyOnWriteArrayList<(JSONObject) -> Unit>()
    // H6.1: outbound AgentTalk req_id -> agent id, matched against the
    // AgentTalkOk reply to learn which pane belongs to which agent
    private val talkPending = java.util.concurrent.ConcurrentHashMap<String, String>()

    private val realtime = Realtime(
        machineId,
        auth,
        onFrame = { frame: JSONObject ->
            // keep the session list fresh (HelloOk / SessionsAck / exited)
            refreshSessions(frame)
            trackAsks(frame)
            for (s in sinks) s(frame)
            notify.onFrame(frame, Monitor.machineName.ifEmpty { "agent" })
        },
        onStatus = { status: String -> Monitor.status = status },
    )

    fun start() = realtime.start()
    fun stop() = realtime.stop()

    // ---- frame bus ----
    fun addSink(s: (JSONObject) -> Unit) = sinks.addIfAbsent(s)
    fun removeSink(s: (JSONObject) -> Unit) = sinks.remove(s)

    // ---- client -> daemon send helpers (all route through the WS) ----
    fun send(frame: JSONObject) {
        if (frame.optString("t") == "AgentTalk") {
            val rid = frame.optString("req_id")
            val agent = frame.optString("agent")
            if (rid.isNotEmpty() && agent.isNotEmpty()) talkPending[rid] = agent
        }
        realtime.sendFrame(frame)
    }
    fun attach(sessionId: String, pane: String? = null) =
        send(Term.attach(sessionId, pane))
    fun detach() = send(Term.detach())
    fun input(sessionId: String, pane: String, text: String) =
        send(Term.input(sessionId, pane, text))
    fun resize(sessionId: String, cols: Int, rows: Int) =
        send(Term.resize(sessionId, cols, rows))
    fun chatSend(sessionId: String, pane: String, text: String, attachments: List<String> = emptyList()) =
        send(Term.chatSend(sessionId, pane, text, attachments))
    fun createSession(kind: String = "shell") =
        send(Term.sessionsCreate(kind))

    private fun trackAsks(f: JSONObject) {
        when (f.optString("t")) {
            "AgentAskRequest" -> {
                val sid = f.optString("session")
                val q = f.optString("question")
                val askId = f.optString("ask_id")
                if (sid.isNotEmpty() && askId.isNotEmpty()) Monitor.pendingAsks[askId] = sid to q
            }
            "AgentAskAnswer" -> Monitor.pendingAsks.remove(f.optString("ask_id"))
            "Meta" -> if (f.optString("kind") == "exited") {
                val sid = f.optString("session")
                Monitor.pendingAsks.values.removeIf { it.first == sid }
            }
        }
    }

    private fun refreshSessions(frame: JSONObject) {
        when (val t = frame.optString("t")) {
            "AgentListOk" -> {
                // H6.1: keep the agent registry (names + avatars) fresh
                // for notification identity; any client's list request
                // refreshes it. Warm the avatar cache too.
                val arr = frame.optJSONArray("agents") ?: return
                for (i in 0 until arr.length()) {
                    val a = arr.getJSONObject(i)
                    val id = a.optString("id")
                    val name = a.optString("name")
                    if (id.isNotEmpty() && name.isNotEmpty()) Monitor.agentNames[id] = name
                    val url = a.optString("avatar_url")
                    if (url.isNotEmpty()) Monitor.agentAvatars[id] = url else
                        Monitor.agentAvatars.remove(id)
                }
                Avatars.prefetch(Monitor.agentAvatars.values.toTypedArray())
            }
            "AgentTalkOk" -> {
                // H6.1: bind the new pane/session to its agent so
                // notifications can carry agent identity
                val agentId = frame.optString("req_id").let { talkPending.remove(it) }
                val pane = frame.optString("pane")
                val sid = frame.optString("session")
                if (agentId != null) {
                    val name = Monitor.agentNames[agentId] ?: agentId
                    if (pane.isNotEmpty()) Monitor.paneAgent[pane] = name
                    if (sid.isNotEmpty()) Monitor.sessionAgent[sid] = name
                }
            }
            "HelloOk" -> {
                Monitor.daemonVersion = frame.optString("version", "")
                Monitor.publishSessions(Term.parseSessionList(frame))
            }
            "SessionsAck" -> {
                val sid = frame.optString("session")
                val pane = frame.optString("pane")
                if (sid.isNotEmpty()) {
                    val created = Term.SessionMeta(
                        id = sid, name = sid.take(8), kind = "",
                        activePane = pane, panes = listOf(pane)
                    )
                    Monitor.publishSessions(Monitor.sessions.filterNot { it.id == created.id } + created)
                    // The ack carries no kind, so the new row briefly shows as a
                    // shell. The daemon inserts the session before acking, so a
                    // Hello now returns the authoritative list with the real
                    // kind (pi/forge) — fixes the badge immediately.
                    send(Term.hello())
                }
            }
            "Meta" -> {
                val kind = frame.optString("kind")
                if (kind == "exited") {
                    val sid = frame.optString("session")
                    if (sid.isNotEmpty())
                        Monitor.publishSessions(Monitor.sessions.filterNot { it.id == sid })
                } else if (kind == "agent") {
                    // working/idle is broadcast to every client, so the
                    // session list stays live without re-helloing
                    val sid = frame.optString("session")
                    if (sid.isNotEmpty()) {
                        val busy = frame.optString("status") == "working"
                        Monitor.publishSessions(
                            Monitor.sessions.map { if (it.id == sid) it.copy(busy = busy) else it }
                        )
                    }
                }
            }
        }
    }
}
