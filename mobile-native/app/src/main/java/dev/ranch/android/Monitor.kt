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

    /** Version string the daemon reported in HelloOk ("" = unknown). */
    @Volatile
    var daemonVersion: String = ""

    fun publishSessions(s: List<Term.SessionMeta>) { sessions = s }

    fun start(app: App, machineId: String, machineName: String) {
        stop()
        this.machineId = machineId
        this.machineName = machineName
        sessions = emptyList()
        daemonVersion = ""
        val s = RelaySession(app, machineId)
        session = s
        running = true
        s.start()
    }

    fun stop() {
        session?.stop()
        session = null
        running = false
        status = "stopped"
        sessions = emptyList()
        daemonVersion = ""
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

    private val realtime = Realtime(
        machineId,
        auth,
        onFrame = { frame: JSONObject ->
            // keep the session list fresh (HelloOk / SessionsAck / exited)
            refreshSessions(frame)
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
    fun send(frame: JSONObject) = realtime.sendFrame(frame)
    fun attach(sessionId: String, pane: String? = null) =
        send(Term.attach(sessionId, pane))
    fun detach() = send(Term.detach())
    fun input(sessionId: String, pane: String, text: String) =
        send(Term.input(sessionId, pane, text))
    fun resize(sessionId: String, cols: Int, rows: Int) =
        send(Term.resize(sessionId, cols, rows))
    fun chatSend(sessionId: String, pane: String, text: String) =
        send(Term.chatSend(sessionId, pane, text))
    fun createSession(kind: String = "shell") =
        send(Term.sessionsCreate(kind))

    private fun refreshSessions(frame: JSONObject) {
        when (val t = frame.optString("t")) {
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
                }
            }
            "Meta" -> {
                if (frame.optString("kind") == "exited") {
                    val sid = frame.optString("session")
                    if (sid.isNotEmpty())
                        Monitor.publishSessions(Monitor.sessions.filterNot { it.id == sid })
                }
            }
        }
    }
}
