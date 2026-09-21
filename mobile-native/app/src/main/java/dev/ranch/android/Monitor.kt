package dev.ranch.android

import org.json.JSONObject

/**
 * Singleton that tracks the active monitoring session and exposes
 * diagnostics to the UI without holding a service reference.
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

    private var session: RelaySession? = null

    fun start(app: App, machineId: String, machineName: String) {
        stop()
        this.machineId = machineId
        this.machineName = machineName
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
    }

    val notify: Notify? get() = session?.notify

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
            "skipActive" to (n?.skippedActive ?: 0),
            "skipOff" to (n?.skippedOff ?: 0),
            "errors" to (n?.errors ?: 0),
        )
    }
}

/**
 * A single monitoring session: one machine, one Realtime channel,
 * one Notify engine.
 */
class RelaySession(
    private val app: App,
    machineId: String,
) {
    val auth: Auth = Auth(app.prefs)
    val notify: Notify = Notify(app)

    private val realtime = Realtime(
        machineId,
        auth,
        onFrame = { frame: JSONObject ->
            notify.onFrame(frame, Monitor.machineName.ifEmpty { "agent" })
        },
        onStatus = { status: String ->
            Monitor.status = status
        },
    )

    fun start() {
        realtime.start()
    }

    fun stop() {
        realtime.stop()
    }
}
