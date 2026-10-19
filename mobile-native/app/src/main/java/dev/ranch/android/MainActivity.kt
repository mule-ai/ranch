package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.util.concurrent.Executors

/**
 * Staged flow, mirroring the RN app (login → machine list → session list):
 *
 *   1. **Login** — email/password + Google (deep-link return).
 *   2. **Machine pick** — big tappable rows (name + online dot); tapping a
 *      machine starts the foreground monitor service immediately.
 *   3. **Monitor home** — live session list (the primary surface), new
 *      session buttons, and one entry point into [SettingsActivity] for
 *      notification toggles / tools / diagnostics.
 */
class MainActivity : Activity() {

    private val app get() = application as App

    // lazy: field initializers run at construction, before Activity.attach(),
    // where getApplication() is still null (NPE = crash on launch)
    private val auth by lazy { Auth(app.prefs) }
    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()

    private lateinit var stateBox: LinearLayout
    private lateinit var bannerBox: LinearLayout
    private var sessionsBox: LinearLayout? = null
    private var homeStatus: TextView? = null
    // Herd-on-home (2026-10-19): the registry lives right on the main
    // screen — before, finding your agents meant digging through
    // sessions (which include one-off chats) or the Settings tools
    // drawer. Rows come from AgentListOk (forge registry via the
    // monitored daemon); tap = AgentTalk{continue}.
    private var herdBox: LinearLayout? = null
    private val herdAgents = ArrayList<Term.AgentInfo>()
    /** AgentTalk req_id -> agent name while a row tap is in flight. */
    private val pendingHerdTalk = HashMap<String, String>()
    private var monitorWanted = false
    private val machines = mutableListOf<Machine>()
    private var update: Version.Update? = null

    // New-agent dialog + working-directory picker (RN parity)
    private var agentKind = "pi"
    private var agentDir = ""          // "" = $HOME (daemon default)
    private var pickerDialog: android.app.AlertDialog? = null
    private var pickerPath = ""
    private var pickerParent: String? = null
    private var pickerPathText: TextView? = null
    private var pickerList: LinearLayout? = null
    private var pickerUp: Button? = null
    private var dirReqId: String? = null
    private var frameSink: ((JSONObject) -> Unit)? = null
    // the RelaySession the sink is registered on — Monitor.start() replaces
    // the session (stop + re-pick, service restart), and a stale reference
    // would silently stop receiving DirListOk (empty directory picker)
    private var frameSinkRelay: RelaySession? = null

    private val refreshRunnable = object : Runnable {
        override fun run() {
            refreshSessions()
            handler.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // monitoring survives app restarts: remember it across process death
        // (APK updates / Android reclaiming memory stop the service)
        monitorWanted = Monitor.running || app.prefs.getBool("monitor_wanted", false)
        if (monitorWanted && !Monitor.running) {
            val mid = app.prefs.get("machine_id", "")
            if (mid.isNotEmpty()) {
                startForegroundService(Intent(this, MonitorService::class.java)
                    .putExtra("machineId", mid)
                    .putExtra("machineName", app.prefs.get("machine_name", "")))
            } else {
                monitorWanted = false
            }
        }
        pendingOpen = pendingFromIntent(intent)
        pendingAgentFromIntent(intent)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(16), dp(16), dp(16), dp(16))
        }
        setContentView(ScrollView(this).apply { addView(root) })
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        addCrashRow(root)
        bannerBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(bannerBox)
        stateBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(stateBox)
        handleAuthIntent(intent)
        renderState()
    }

    override fun onNewIntent(i: Intent) {
        super.onNewIntent(i)
        pendingFromIntent(i)?.let { pendingOpen = it }
        pendingAgentFromIntent(i)
        handleAuthIntent(i)
        renderState()
    }

    // ---- notification deep link ----
    // Notifications carry open_session/open_session_name/open_pane; once
    // the monitor is up we drop the user straight into that conversation
    // (and that exact pane, for sessions with several chat panes).
    private var pendingOpen: Triple<String, String, String>? = null

    /** H6.1 task-notification buttons: open the agent's conversation
     *  (mode "continue") or fork it (mode "fork") via AgentTalk. */
    private var pendingAgent: Pair<String, String>? = null
    /** When the pending agent first became unresolvable (the registry
     *  may simply not have loaded yet) — bounded retry, then give up. */
    private var agentUnresolvedSince: Long? = null
    private var agentTalkSink: ((JSONObject) -> Unit)? = null

    private fun pendingFromIntent(i: Intent?): Triple<String, String, String>? {
        val sid = i?.getStringExtra("open_session") ?: return null
        if (sid.isEmpty()) return null
        return Triple(sid, i.getStringExtra("open_session_name") ?: "", i.getStringExtra("open_pane") ?: "")
    }

    private fun pendingAgentFromIntent(i: Intent?) {
        val name = i?.getStringExtra("open_agent") ?: return
        if (name.isEmpty()) return
        pendingAgent = name to (i.getStringExtra("open_agent_mode") ?: "continue")
    }

    private fun maybeAutoOpen() {
        val p = pendingOpen
        if (p != null) {
            if (Monitor.running) {
                pendingOpen = null
                startActivity(Intent(this, SessionActivity::class.java)
                    .putExtra("sessionId", p.first)
                    .putExtra("sessionName", p.second)
                    .putExtra("openPane", p.third))
            }
            return
        }
        val a = pendingAgent ?: return
        if (!Monitor.running) return   // retry on the refresh tick once connected
        val agentId = Monitor.agentIdFor(a.first)
        if (agentId == null) {
            // the registry (AgentListOk) may still be loading — retry for
            // up to 15 s, then give up with a toast
            val since = agentUnresolvedSince ?: System.currentTimeMillis().also { agentUnresolvedSince = it }
            if (System.currentTimeMillis() - since > 15_000) {
                pendingAgent = null
                agentUnresolvedSince = null
                Toast.makeText(this, "agent \"${a.first}\" not in the registry", Toast.LENGTH_LONG).show()
            }
            return
        }
        pendingAgent = null
        agentUnresolvedSince = null
        val rid = "at-notif-" + System.currentTimeMillis()
        val agentName = a.first
        val sink: (JSONObject) -> Unit = { f -> onAgentNotifyFrame(f, rid, agentName) }
        agentTalkSink = sink
        Monitor.relay?.addSink(sink)
        Monitor.relay?.send(Term.agentTalk(agentId, a.second, rid))
    }

    private fun onAgentNotifyFrame(f: JSONObject, rid: String, agentName: String) {
        if (f.optString("req_id") != rid) return
        when (f.optString("t")) {
            "AgentTalkOk" -> {
                agentTalkSink?.let { Monitor.relay?.removeSink(it) }
                agentTalkSink = null
                startActivity(Intent(this, SessionActivity::class.java)
                    .putExtra("sessionId", f.optString("session"))
                    .putExtra("sessionName", agentName)
                    .putExtra("openPane", f.optString("pane")))
            }
            "Error" -> {
                agentTalkSink?.let { Monitor.relay?.removeSink(it) }
                agentTalkSink = null
                Toast.makeText(this, f.optString("message", "error"), Toast.LENGTH_LONG).show()
            }
        }
    }

    override fun onResume() {
        super.onResume()
        renderState()
        renderBanner()
        // update check (silent on failure / dev builds)
        exec.execute {
            val u = Version.checkUpdate()
            handler.post {
                val changed = u?.latest != update?.latest
                update = u
                if (changed) renderBanner()
            }
        }
        handler.postDelayed(refreshRunnable, 1000)
    }

    override fun onPause() {
        handler.removeCallbacks(refreshRunnable)
        super.onPause()
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        exec.shutdownNow()
        agentTalkSink?.let { Monitor.relay?.removeSink(it) }
        agentTalkSink = null
        frameSink?.let { frameSinkRelay?.removeSink(it) }
        frameSink = null
        frameSinkRelay = null
        pickerDialog = null
        super.onDestroy()
    }

    // ---- state machine ----
    private fun renderState() {
        if (!::stateBox.isInitialized) return
        stateBox.removeAllViews()
        sessionsBox = null
        homeStatus = null
        when {
            !auth.isLoggedIn() -> renderLogin()
            !monitorWanted && !Monitor.running -> renderMachinePick()
            else -> renderMonitorHome()
        }
    }

    // ---- 1. login ----
    private fun renderLogin() {
        stateBox.addView(TextView(this).apply {
            text = "🤠 Ranch"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 30f)
            setTextColor(0xFFE5E5E5.toInt())
            gravity = Gravity.CENTER
            setPadding(0, dp(48), 0, dp(4))
        })
        stateBox.addView(TextView(this).apply {
            text = "your agents, in your pocket"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFF6B7280.toInt())
            gravity = Gravity.CENTER
            setPadding(0, 0, 0, dp(32))
        })
        val email = EditText(this).apply {
            hint = "email"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        }
        val pw = EditText(this).apply {
            hint = "password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        }
        val status = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }
        stateBox.addView(email)
        stateBox.addView(pw)
        stateBox.addView(Button(this).apply {
            text = "Sign in"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setPadding(0, dp(10), 0, dp(10))
            setOnClickListener {
                status.text = "signing in…"
                exec.execute {
                    val err = auth.login(email.text.toString(), pw.text.toString())
                    handler.post {
                        status.text = if (err == null) "signed in ✓" else "error: $err"
                        if (err == null) renderState()
                    }
                }
            }
        })
        stateBox.addView(Button(this).apply {
            text = "Sign in with Google"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(auth.googleAuthorizeUrl())))
                    status.text = "completing sign-in in browser…"
                } catch (e: Exception) { status.text = "no browser: ${e.message}" }
            }
        })
        stateBox.addView(status)
    }

    // ---- 2. machine pick ----
    private fun renderMachinePick() {
        stateBox.addView(TextView(this).apply {
            text = "Pick a machine"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 22f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, dp(24), 0, dp(4))
        })
        stateBox.addView(TextView(this).apply {
            text = "tapping a machine starts monitoring it"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setPadding(0, 0, 0, dp(12))
        })
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        stateBox.addView(list)
        val status = TextView(this).apply {
            text = "loading machines…"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }
        stateBox.addView(status)
        stateBox.addView(Button(this).apply {
            text = "sign out"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setOnClickListener { auth.logout(); renderState() }
        })
        loadMachines(list, status)
    }

    private fun loadMachines(list: LinearLayout, status: TextView) {
        exec.execute {
            auth.machines().fold(
                onSuccess = { items -> handler.post {
                    machines.clear(); machines.addAll(items)
                    list.removeAllViews()
                    for (m in items) list.addView(machineRow(m))
                    status.text = if (items.isEmpty())
                        "no machines — run 'ranch register' on a daemon host"
                    else "${items.size} machine(s)"
                } },
                onFailure = { e -> handler.post { status.text = "error: ${e.message}" } }
            )
        }
    }

    private fun machineRow(m: Machine): View {
        val online = isOnline(m)
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(14), dp(16), dp(14))
            background = GradientDrawable().apply {
                cornerRadius = dp(12).toFloat()
                setColor(0xFF1C2127.toInt())
            }
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                LinearLayout.LayoutParams.WRAP_CONTENT
            ).apply { setMargins(0, dp(6), 0, dp(6)) }
            setOnClickListener { selectMachine(m) }
        }
        row.addView(TextView(this).apply {
            text = (if (online) "● " else "○ ") + m.name
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 17f)
            setTextColor(if (online) 0xFF4ADE80.toInt() else 0xFF9AA0A6.toInt())
        })
        row.addView(TextView(this).apply {
            text = if (online) "online — tap to monitor"
            else "offline${if (m.lastSeenAt.isNotEmpty()) " · last seen ${m.lastSeenAt}" else ""}"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setPadding(0, dp(2), 0, 0)
        })
        return row
    }

    private fun selectMachine(m: Machine) {
        app.prefs.set("machine_id", m.id)
        app.prefs.set("machine_name", m.name)
        app.prefs.setBool("monitor_wanted", true)
        monitorWanted = true
        startForegroundService(Intent(this, MonitorService::class.java)
            .putExtra("machineId", m.id)
            .putExtra("machineName", m.name))
        renderState()
    }

    // ---- 3. monitor home ----
    private fun renderMonitorHome() {
        ensureFrameSink()
        val head = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
        head.addView(TextView(this).apply {
            text = "🤠 ${Monitor.machineName.ifEmpty { "monitor" }}"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        head.addView(Button(this).apply {
            text = "Stop"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setOnClickListener {
                monitorWanted = false
                app.prefs.setBool("monitor_wanted", false)
                startService(Intent(this@MainActivity, MonitorService::class.java).setAction("stop"))
                renderState()
            }
        })
        stateBox.addView(head)

        homeStatus = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, 0, 0, dp(4))
        }
        stateBox.addView(homeStatus)

        // ---- Herd (forge registry) — first thing on the main screen ----
        val herdHead = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL
        }
        herdHead.addView(TextView(this).apply {
            text = "Herd"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7FD4FF.toInt())
            setPadding(0, dp(12), 0, dp(4))
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        herdHead.addView(Button(this).apply {
            text = "activity"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF4ade80.toInt())
            setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener { startActivity(Intent(this@MainActivity, HerdActivity::class.java)) }
        })
        herdHead.addView(Button(this).apply {
            text = "manage"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF7FD4FF.toInt())
            setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener { startActivity(Intent(this@MainActivity, AgentsActivity::class.java)) }
        })
        stateBox.addView(herdHead)
        val hBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        herdBox = hBox
        stateBox.addView(hBox)
        renderHerd()
        Monitor.relay?.send(Term.agentList())

        stateBox.addView(TextView(this).apply {
            text = "Sessions"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7FD4FF.toInt())
            setPadding(0, dp(12), 0, dp(4))
        })
        val newSessionRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        newSessionRow.addView(Button(this).apply {
            text = "+ shell"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(dp(8), dp(8), dp(8), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener {
                Monitor.relay?.createSession("shell")
                homeStatus?.text = "creating shell session…"
            }
        })
        newSessionRow.addView(Button(this).apply {
            text = "+ agent"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(dp(8), dp(8), dp(8), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener { showNewAgentDialog() }
        })
        stateBox.addView(newSessionRow)
        val box = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        sessionsBox = box
        stateBox.addView(box)
        refreshSessions()

        stateBox.addView(Button(this).apply {
            text = "⚙ settings, tools & diagnostics"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(0, dp(16), 0, dp(8))
            setOnClickListener { startActivity(Intent(this@MainActivity, SettingsActivity::class.java)) }
        })

        maybeAutoOpen()
    }

    private fun refreshSessions() {
        val box = sessionsBox ?: return
        val sessions = Monitor.sessions
        homeStatus?.text = when {
            Monitor.status != "open" && Monitor.status != "monitoring" && !Monitor.running -> "disconnected"
            sessions.isEmpty() -> "no sessions yet"
            else -> "${sessions.size} session(s) · ${Monitor.status}"
        }
        box.removeAllViews()
        for (s in sessions) {
            box.addView(Button(this).apply {
                val badge = when (s.kind) {
                    "forge" -> "🤖 agent"
                    "pi" -> "π pi"
                    else -> "💻 shell"
                }
                val mark = if (s.busy) "⚙ working · " else ""
                val dir = s.cwd?.let { abbrevPath(it) }
                text = "${mark}${s.name.ifEmpty { s.id.take(8) }}\n${badge}${dir?.let { " · $it" } ?: ""}"
                setTextColor(if (s.busy) 0xFFF59E0B.toInt() else 0xFFE5E5E5.toInt())
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
                setPadding(dp(14), dp(12), dp(14), dp(12))
                setOnClickListener {
                    startActivity(Intent(this@MainActivity, SessionActivity::class.java)
                        .putExtra("sessionId", s.id)
                        .putExtra("sessionName", s.name))
                }
            })
        }
        maybeAutoOpen()
    }

    /** Truncate a path to its last 33 chars for the row (leading …). */
    private fun abbrevPath(p: String): String =
        if (p.length <= 33) p else "…" + p.takeLast(33)

    // ---- Herd section (2026-10-19) ----

    /** Registry rows on the home screen. Tap opens the agent's
     *  most-active conversation (AgentTalk continue); the 📞/✎ buttons
     *  dial and edit; long-press offers pause/resume (the H5.3 kill
     *  switch, per agent). Newest-activity-first (AgentListOk sort). */
    private fun renderHerd() {
        val box = herdBox ?: return
        box.removeAllViews()
        if (herdAgents.isEmpty()) {
            box.addView(TextView(this).apply {
                text = "no agents in the herd yet — provision one, or start a one-off chat below"
                setTextColor(0xFF6b7280.toInt())
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setPadding(dp(4), dp(4), 0, dp(4))
            })
            box.addView(Button(this).apply {
                text = "＋ provision herd agent"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setOnClickListener {
                    startActivity(Intent(this@MainActivity, NewAgentActivity::class.java))
                }
            })
            return
        }
        for (a in herdAgents) box.addView(herdRow(a))
    }

    private fun herdRow(a: Term.AgentInfo): LinearLayout {
        val paused = a.paused == true
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(6), dp(8), dp(6), dp(8))
            setOnClickListener { openHerdAgent(a) }
            setOnLongClickListener {
                val openAct: () -> Unit = { openHerdAgent(a) }
                val pauseAct: () -> Unit = {
                    Monitor.relay?.send(Term.agentPause(a.name, !paused, "hp-" + Term.newId()))
                    homeStatus?.text = if (paused) "resuming ${a.name}…" else "pausing ${a.name}…"
                }
                val editAct: () -> Unit = {
                    startActivity(Intent(this@MainActivity, EditAgentActivity::class.java)
                        .putExtra(EditAgentActivity.EXTRA_AGENT, herdRowJson(a).toString()))
                }
                val callAct: () -> Unit = {
                    startActivity(Intent(this@MainActivity, CallActivity::class.java)
                        .putExtra(CallActivity.EXTRA_AGENT_ID, a.id)
                        .putExtra(CallActivity.EXTRA_AGENT_NAME, a.name))
                }
                val actions = listOf(
                    "open chat" to openAct,
                    (if (paused) "resume" else "pause") to pauseAct,
                    "edit" to editAct,
                    "call" to callAct,
                )
                android.app.AlertDialog.Builder(this@MainActivity)
                    .setTitle(a.name.ifEmpty { a.id.take(8) })
                    .setItems(actions.map { it.first }.toTypedArray()) { _, i -> actions[i].second() }
                    .show()
                true
            }
        }
        val initial = (a.avatarUrl?.lastOrNull { it.isLetterOrDigit() } ?: '?').toString().uppercase()
        val tint = Math.abs((a.name + a.id).hashCode() % 0xFFFFFF).toInt().or(0xFF304050.toInt())
        row.addView(TextView(this).apply {
            text = initial
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFFFFFFF.toInt())
            gravity = Gravity.CENTER
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL; setColor(tint)
            }
            layoutParams = LinearLayout.LayoutParams(dp(36), dp(36))
        })
        val mid = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(10), 0, dp(10), 0)
        }
        mid.addView(TextView(this).apply {
            text = (if (paused) "⏸ " else "") + a.name.ifEmpty { a.id.take(8) }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(if (paused) 0xFF6b7280.toInt() else 0xFFE5E5E5.toInt())
        })
        val sub = StringBuilder()
        a.homeMachine?.let { sub.append("on ").append(if (it.length <= 18) it else "…" + it.takeLast(18)) }
        a.conversationCount?.let { sub.append(" · ").append(it).append(" conv") }
        a.lastActive?.let {
            if (sub.isNotEmpty()) sub.append(" · ")
            sub.append("last ").append(herdAgo(it))
        }
        if (sub.isNotEmpty()) mid.addView(TextView(this).apply {
            text = sub.toString()
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6b7280.toInt())
        })
        row.addView(mid, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        row.addView(Button(this).apply {
            text = "📞"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            minWidth = dp(40); minimumWidth = dp(40); minHeight = dp(40)
            setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener {
                startActivity(Intent(this@MainActivity, CallActivity::class.java)
                    .putExtra(CallActivity.EXTRA_AGENT_ID, a.id)
                    .putExtra(CallActivity.EXTRA_AGENT_NAME, a.name))
            }
        })
        row.addView(Button(this).apply {
            text = "✎"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7FD4FF.toInt())
            minWidth = dp(40); minimumWidth = dp(40); minHeight = dp(40)
            setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener {
                startActivity(Intent(this@MainActivity, EditAgentActivity::class.java)
                    .putExtra(EditAgentActivity.EXTRA_AGENT, herdRowJson(a).toString()))
            }
        })
        return row
    }

    private fun openHerdAgent(a: Term.AgentInfo) {
        val frame = Term.agentTalk(a.id, "continue", "at-" + Term.newId())
        pendingHerdTalk[frame.getString("req_id")] = a.name.ifEmpty { a.id.take(8) }
        Monitor.relay?.send(frame)
        homeStatus?.text = "opening ${a.name.ifEmpty { a.id.take(8) }}…"
    }

    /** RFC3339 -> "3m ago" / "5h ago" / "11-19 14:02" (best-effort,
     *  no parsing libs on this screen). */
    private fun herdAgo(ts: String): String {
        if (ts.length < 16 || ts[10] != 'T') return ts.take(16)
        return try {
            val then = java.time.OffsetDateTime.parse(ts)
            val mins = java.time.Duration.between(then, java.time.OffsetDateTime.now()).toMinutes()
            when {
                mins < 1 -> "just now"
                mins < 60 -> "${mins}m ago"
                mins < 60 * 24 -> "${mins / 60}h ago"
                mins < 60 * 24 * 7 -> "${mins / (60 * 24)}d ago"
                else -> ts.substring(5, 10) + " " + ts.substring(11, 16)
            }
        } catch (_: Exception) {
            ts.take(16)
        }
    }

    /** Row as the wire JSON the editor prefills from (same field
     *  names AgentListOk carries — mirrors AgentsActivity). */
    private fun herdRowJson(a: Term.AgentInfo): JSONObject = JSONObject()
        .put("id", a.id)
        .put("name", a.name)
        .apply {
            a.avatarUrl?.let { put("avatar_url", it) }
            a.homeMachine?.let { put("home_machine", it) }
            a.primaryProfile?.let { put("primary_profile", it) }
            a.memoryScope?.let { put("memory_scope", it) }
            a.orgId?.let { put("org_id", it) }
            a.visibility?.let { put("visibility", it) }
            a.toolsAllowlist?.let { l -> put("tools_allowlist", org.json.JSONArray(l)) }
            a.extraInstructions?.let { put("extra_instructions", it) }
            a.paused?.let { put("paused", it) }
            a.envRefs?.let { put("env_refs", org.json.JSONArray(it)) }
        }

    // ---- new agent dialog: kind + name + working-dir picker (RN parity) ----

    private fun ensureFrameSink() {
        val relay = Monitor.relay ?: return
        if (frameSink != null && frameSinkRelay === relay) return
        // re-register on the new session (drop the stale one first)
        frameSink?.let { old -> frameSinkRelay?.removeSink(old) }
        val sink: (JSONObject) -> Unit = { f ->
            when (f.optString("t")) {
                "DirListOk" -> handler.post { onDirListOk(f) }
                "AgentListOk" -> handler.post {
                    herdAgents.clear()
                    f.optJSONArray("agents")?.let { a ->
                        for (i in 0 until a.length())
                            herdAgents.add(Term.parseAgentInfo(a.getJSONObject(i)))
                    }
                    // most-recently-active first; nulls last
                    herdAgents.sortByDescending { it.lastActive ?: "" }
                    renderHerd()
                }
                "AgentTalkOk" -> handler.post {
                    val name = pendingHerdTalk.remove(f.optString("req_id")) ?: return@post
                    startActivity(Intent(this@MainActivity, SessionActivity::class.java)
                        .putExtra("sessionId", f.optString("session"))
                        .putExtra("sessionName", name))
                }
                "AgentPauseOk" -> handler.post {
                    val rid = f.optString("req_id")
                    if (rid.startsWith("hp-")) {
                        pendingHerdTalk.remove(rid)
                        homeStatus?.text = "pause state updated"
                        Monitor.relay?.send(Term.agentList())
                    }
                }
                // Create-error surfacing: SessionsCreate failures (e.g.
                // "pi spawn failed: No such file or directory") used to
                // leave the "creating …" status hanging forever.
                "Error" -> {
                    val rid = f.optString("req_id", "")
                    if (rid.startsWith("sc-")) {
                        handler.post { homeStatus?.text = "create failed: ${f.optString("message", "unknown error")}" }
                    } else if (rid.startsWith("at-") || rid.startsWith("hp-")) {
                        handler.post { homeStatus?.text = "herd: ${f.optString("message", "error")}" }
                    }
                }
            }
        }
        frameSink = sink
        frameSinkRelay = relay
        relay.addSink(sink)
    }

    private fun showNewAgentDialog() {
        val relay = Monitor.relay ?: return
        ensureFrameSink()
        agentDir = ""
        agentKind = "pi"

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(12), dp(16), dp(8))
        }
        val kindRow = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        var piChip: Button? = null
        var forgeChip: Button? = null
        var dirRow: LinearLayout? = null
        piChip = agentChip(
            "π one-off chat — pi on THIS machine",
            "Runs a local pi agent in a folder on ${Monitor.machineName}. No registry, no wake — just a chat that lives on this machine.",
        ) {
            agentKind = "pi"; styleAgentChips(piChip!!, forgeChip!!, dirRow!!)
        }
        forgeChip = agentChip(
            "🤖 one-off chat — lab forge model",
            "A conversation on a lab forge profile (default profile). Lives in forge, not on this machine.",
        ) {
            agentKind = "forge"; styleAgentChips(piChip!!, forgeChip!!, dirRow!!)
        }
        kindRow.addView(piChip)
        kindRow.addView(forgeChip)
        root.addView(kindRow)

        val nameEdit = EditText(this).apply {
            hint = "name (optional)"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        }
        root.addView(nameEdit, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT
        ).apply { setMargins(0, dp(8), 0, 0) })

        dirRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, dp(8), 0, 0)
        }
        var dirChip: Button? = null
        dirChip = agentChip("dir: \$HOME", "pick the folder the pi agent works in") { showDirPicker(dirChip!!) }
        dirRow!!.addView(dirChip!!)
        root.addView(dirRow)

        styleAgentChips(piChip!!, forgeChip!!, dirRow!!)

        android.app.AlertDialog.Builder(this)
            .setTitle("New one-off chat")
            .setView(root)
            .setPositiveButton("create") { _, _ ->
                val name = nameEdit.text.toString().trim().ifEmpty { null }
                val cwd = if (agentKind == "pi") agentDir.ifEmpty { null } else null
                val reqId = "sc-" + Term.newId()
                relay.send(Term.sessionsCreate(agentKind, name, cwd, reqId))
                homeStatus?.text = "creating ${if (agentKind == "pi") "local pi" else "forge"} chat…"
            }
            .setNegativeButton("cancel", null)
            .show()
    }

    private fun styleAgentChips(piChip: Button, forgeChip: Button, dirRow: LinearLayout) {
        val isPi = agentKind == "pi"
        styleChip(piChip, isPi)
        styleChip(forgeChip, !isPi)
        dirRow.visibility = if (isPi) View.VISIBLE else View.GONE
    }

    private fun styleChip(b: Button, on: Boolean) {
        b.setTextColor(if (on) 0xFFFFFFFF.toInt() else 0xFF7FD4FF.toInt())
        b.setBackgroundColor(if (on) 0xFF1B6FD4.toInt() else 0xFF1B2126.toInt())
    }

    private fun agentChip(label: String, subtitle: String, click: () -> Unit): Button =
        Button(this).apply {
            text = "$label\n$subtitle"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setPadding(dp(10), dp(8), dp(10), dp(8))
            minWidth = 0
            isAllCaps = false
            gravity = Gravity.START or Gravity.CENTER_VERTICAL
            setOnClickListener { click() }
        }

    private fun showDirPicker(dirChip: Button) {
        val relay = Monitor.relay ?: return
        ensureFrameSink()
        pickerPath = ""
        pickerParent = null

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(8), dp(16), dp(8))
        }
        pickerPathText = TextView(this).apply {
            text = "\$HOME"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, 0, 0, dp(6))
        }
        root.addView(pickerPathText!!)

        pickerList = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(pickerList!!) },
            LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, dp(220)
            ).apply { setMargins(0, 0, 0, dp(8)) })

        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val up = agentChip("up…", "") { pickerParent?.let { browseDir(it) } }
        row.addView(up)
        row.addView(agentChip("use this dir", "") {
            agentDir = pickerPath
            dirChip.text = "dir: " + (agentDir.ifEmpty { "\$HOME" })
            pickerDialog?.dismiss()
        })
        row.addView(agentChip("cancel", "") { pickerDialog?.dismiss() })
        root.addView(row)
        this@MainActivity.pickerUp = up

        pickerDialog = android.app.AlertDialog.Builder(this)
            .setTitle("choose working directory")
            .setView(root)
            .create()
        pickerDialog?.show()
        browseDir(null)
    }

    private fun browseDir(path: String?) {
        val relay = Monitor.relay ?: return
        val frame = Term.dirList(path)
        dirReqId = frame.optString("req_id")
        relay.send(frame)
    }

    private fun onDirListOk(f: JSONObject) {
        if (f.optString("req_id") != dirReqId) return
        dirReqId = null
        pickerPath = f.optString("path")
        pickerParent = f.optString("parent").takeIf { it.isNotEmpty() }
        val dirs = f.optJSONArray("dirs")
            ?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList()
        pickerPathText?.text = pickerPath
        pickerUp?.visibility = if (pickerParent != null) View.VISIBLE else View.GONE
        val list = pickerList ?: return
        list.removeAllViews()
        if (dirs.isEmpty()) {
            list.addView(TextView(this).apply {
                text = "no subdirectories"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF6B7280.toInt())
            })
        }
        for (d in dirs) {
            list.addView(Button(this).apply {
                text = "$d/"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFE5E5E5.toInt())
                setPadding(dp(8), dp(8), dp(8), dp(8))
                setOnClickListener { browseDir("$pickerPath/$d") }
            })
        }
    }

    // ---- shared ----
    /** Out-of-date banner + daemon-version note (port of the RN banner). */
    private fun renderBanner() {
        if (!::bannerBox.isInitialized) return
        bannerBox.removeAllViews()
        update?.let { u ->
            bannerBox.addView(Button(this).apply {
                text = "⬆ update available — download ${u.latest}"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setPadding(dp(10), dp(10), dp(10), dp(10))
                layoutParams = LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT,
                    LinearLayout.LayoutParams.WRAP_CONTENT
                ).apply { setMargins(0, dp(4), 0, dp(4)) }
                setOnClickListener {
                    try {
                        startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(u.apkUrl)))
                    } catch (e: Exception) {
                        Toast.makeText(this@MainActivity, "no browser: ${e.message}", Toast.LENGTH_LONG).show()
                    }
                }
            })
        }
        val dv = Monitor.daemonVersion
        if (update != null && dv.isNotEmpty() && dv != "dev" && dv != update?.latest) {
            bannerBox.addView(TextView(this).apply {
                text = "daemon runs $dv — latest is ${update?.latest}; update the daemon (\"ranch upgrade\" or reinstall)"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFFF59E0B.toInt())
                setPadding(dp(4), dp(2), dp(4), dp(6))
            })
        }
    }

    private fun addCrashRow(root: LinearLayout) {
        val crashFile = java.io.File(filesDir, "crash.txt")
        if (!crashFile.exists()) return
        root.addView(Button(this).apply {
            text = "⚠ view last crash log"
            setTextColor(0xFFEF4444.toInt())
            setOnClickListener {
                val text = crashFile.readText()
                val tv = TextView(this@MainActivity).apply {
                    this.text = text
                    setTextIsSelectable(true)
                    typeface = android.graphics.Typeface.MONOSPACE
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                    setTextColor(0xFFE5E5E5.toInt())
                    setPadding(dp(16), dp(12), dp(16), dp(12))
                }
                val box = ScrollView(this@MainActivity).apply {
                    isVerticalScrollBarEnabled = true
                    setBackgroundColor(0xFF14181D.toInt())
                    addView(tv)
                }
                android.app.AlertDialog.Builder(this@MainActivity)
                    .setTitle("last crash")
                    .setView(box)
                    .setNeutralButton("copy") { _, _ ->
                        val cm = getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                            as android.content.ClipboardManager
                        cm.setPrimaryClip(android.content.ClipData
                            .newPlainText("ranch crash log", text))
                        Toast.makeText(this@MainActivity, "copied — paste it into the bug report", Toast.LENGTH_SHORT).show()
                    }
                    .setPositiveButton("clear") { _, _ -> crashFile.delete() }
                    .setNegativeButton("close", null)
                    .show()
            }
        })
    }

    private fun handleAuthIntent(i: Intent?) {
        val data = i?.data ?: return
        if (data.scheme != "ranch" || data.host != "auth-callback") return
        val err = auth.applyOAuthFragment(data.fragment)
        if (err == null) renderState()
        else Toast.makeText(this, "google sign-in failed: $err", Toast.LENGTH_LONG).show()
    }

    private fun isOnline(m: Machine): Boolean {
        if (m.lastSeenAt.isEmpty()) return false
        return try {
            val t = java.time.Instant.parse(m.lastSeenAt).toEpochMilli()
            System.currentTimeMillis() - t < 90_000
        } catch (_: Exception) { false }
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}
