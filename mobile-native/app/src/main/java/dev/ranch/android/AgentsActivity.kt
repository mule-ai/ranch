package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import org.json.JSONObject

/**
 * Herd H1.4 — Agent registry + local pi manager.
 *
 * Top section: the forge agent registry. `AgentList` -> `AgentListOk`
 * renders one row per agent (avatar placeholder, name, live busy dot
 * from `AgentBusy`/`AgentBusyOk`, home machine + last active). Tapping
 * a row sends `AgentTalk{mode:"continue"}`; on `AgentTalkOk` the new
 * session/pane opens in [SessionActivity] — the same extras the home
 * screen session list uses. `Error` replies (unknown agent, agent lives
 * on another machine, forge down) flash the status line; they never
 * crash the screen (M8.4 "flash, don't die").
 *
 * Bottom section (under a "Local pi sessions" divider): the Phase-4
 * local pi manager — `PiList`/`PiListOk`, the `PiMonitor` toggle, and
 * "adopt into ranch" — unchanged from the pre-H1.4 screen.
 */
class AgentsActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private lateinit var agentList: LinearLayout
    private lateinit var piList: LinearLayout
    private lateinit var monitorSw: Switch
    private var monitorOn = false
    /** agent id -> busy-dot TextView (updated by AgentBusyOk). */
    private val agentDots = HashMap<String, TextView>()
    /** AgentTalk req_id -> (agent name, target agent id) while in flight. */
    private val pendingTalk = HashMap<String, Term.AgentInfo>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val r = Monitor.relay
        if (r == null) { setContentView(errorView("Start monitoring first.")); return }
        relay = r

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(12), dp(12), dp(12), dp(12))
        }
        val bar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val back = Button(this).apply { text = "←"; setOnClickListener { finish() } }
        val title = TextView(this).apply {
            text = "Agents"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }
        bar.addView(back)
        bar.addView(title, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "loading registry…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        // ---- forge agent registry ----
        root.addView(TextView(this).apply {
            text = "HERD AGENTS — registered agents (name, model, memory, wakes). Tap one to talk; ＋ creates a new one."
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6B7280.toInt()); setPadding(0, dp(2), 0, dp(4))
        })
        agentList = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(agentList)
        // H6.5: the ＋Agent form (mobile twin of `ranch agents new`).
        root.addView(Button(this).apply {
            text = "＋ New agent"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                startActivity(Intent(this@AgentsActivity, NewAgentActivity::class.java))
            }
        })

        // ---- divider: local pi manager ----
        root.addView(TextView(this).apply {
            text = "ONE-OFF PI CHATS — disposable local pi sessions on ${Monitor.machineName} (not in the registry)"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(12), 0, dp(4))
        })

        monitorSw = Switch(this).apply {
            text = "Watch external pi sessions"
            setOnCheckedChangeListener { _, c ->
                monitorOn = c
                relay?.send(Term.piMonitor(c))
            }
        }
        root.addView(monitorSw)

        root.addView(Button(this).apply {
            text = "＋ New one-off pi chat"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                val reqId = "sc-" + Term.newId()
                relay?.send(Term.sessionsCreate("pi", null, null, reqId))
                status.text = "creating one-off pi chat…"
            }
        })

        piList = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(piList) })

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.agentList())
        r.send(Term.piList())
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "AgentListOk" -> {
                val agents = f.optJSONArray("agents")
                val n = agents?.length() ?: 0
                status.text = if (n == 0) "no agents registered" else "$n agent(s)"
                agentList.removeAllViews()
                agentDots.clear()
                agents?.let { a ->
                    for (i in 0 until a.length()) {
                        val info = Term.parseAgentInfo(a.getJSONObject(i))
                        agentList.addView(agentRow(info))
                        // busy dot: one round trip per agent (the herd
                        // is small; the dot is the H1.4 "who's working"
                        // glance). The req id doubles as the join key —
                        // AgentBusyOk carries no agent field on the wire.
                        relay?.send(Term.agentBusy(info.id, "ab-" + info.id))
                    }
                }
                if (n == 0) agentList.addView(hint("no agents registered in forge"))
            }
            "AgentBusyOk" -> {
                val dot = agentDots[f.optString("req_id").removePrefix("ab-")]
                val busy = f.optBoolean("busy", false)
                if (dot != null) {
                    dot.text = if (busy) "●" else "○"
                    dot.setTextColor(if (busy) 0xFFF59E0B.toInt() else 0xFF4b5563.toInt())
                }
            }
            "AgentTalkOk" -> {
                val pending = pendingTalk.remove(f.optString("req_id"))
                if (pending != null) {
                    val sid = f.optString("session")
                    status.text = "opening ${pending.name}…"
                    startActivity(Intent(this, SessionActivity::class.java)
                        .putExtra("sessionId", sid)
                        .putExtra("sessionName", pending.name))
                }
            }
            "AgentProvisionOk" -> {
                // The ＋Agent form reports on its own screen; here just
                // refresh the registry so the new dot shows up.
                relay?.send(Term.agentList())
            }
            "AgentDeleteOk" -> {
                flash("agent deleted", green = true)
                relay?.send(Term.agentList())
            }
            "Error" -> {
                val rid = f.optString("req_id")
                val msg = f.optString("message", "error")
                when {
                    rid.startsWith("al-") -> flash("registry: $msg")
                    rid.startsWith("ab-") -> { /* a busy probe failing is noise */ }
                    rid.startsWith("at-") -> flash("talk: $msg")
                    rid.startsWith("ad-") -> flash("delete: $msg")
                    rid.startsWith("pil-") -> flash("pi list: $msg")
                    rid.startsWith("sc-") -> status.text = "create failed: $msg"
                    else -> { /* SessionsCreate & friends report via SessionsAck */ }
                }
            }
            "PiListOk" -> {
                val sessions = f.optJSONArray("sessions")
                val n = sessions?.length() ?: 0
                piList.removeAllViews()
                sessions?.let { a ->
                    for (i in 0 until a.length()) {
                        val s = Term.parsePiSession(a.getJSONObject(i))
                        piList.addView(piRow(s))
                    }
                }
                if (n == 0) piList.addView(hint("no local pi sessions"))
            }
            "PiMonitorOk" -> {
                monitorOn = f.optBoolean("enabled", false)
                monitorSw.isChecked = monitorOn
            }
            "SessionsAck" -> {
                flash("adopted — see the session list on the home screen")
            }
        }
    }

    /** Flash the status line (amber, "flash, don't die"). */
    private fun flash(msg: String, green: Boolean = false) {
        status.text = msg
        status.setTextColor(if (green) 0xFF34d399.toInt() else 0xFFF59E0B.toInt())
        handler.postDelayed({
            status.setTextColor(0xFF9aa0a6.toInt())
            if (green) status.text = ""
        }, 4000)
    }

    // ---- forge agent rows ----

    private fun agentRow(a: Term.AgentInfo): LinearLayout {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(10), dp(10), dp(10), dp(10))
            setOnClickListener {
                val frame = Term.agentTalk(a.id, "continue")
                val reqId = frame.getString("req_id")
                pendingTalk[reqId] = a
                relay?.send(frame)
                flash("opening ${a.name.ifEmpty { a.id.take(8) }}…")
            }
        }
        row.addView(avatar(a))
        val mid = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(10), 0, dp(10), 0)
        }
        val name = TextView(this).apply {
            text = a.name.ifEmpty { a.id.take(8) }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(0xFFE5E5E5.toInt())
        }
        mid.addView(name)
        val sub = StringBuilder()
        a.homeMachine?.let { sub.append("on ${abbrev(it)}") }
        a.lastActive?.let {
            if (sub.isNotEmpty()) sub.append(" · ")
            sub.append("last ${it.take(16)}")
        }
        if (sub.isNotEmpty()) {
            mid.addView(TextView(this).apply {
                text = sub.toString()
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(0xFF6b7280.toInt())
            })
        }
        row.addView(mid, LinearLayout.LayoutParams(0, 0, 1f))
        // H6.3: the 📞 button opens the voice call (CallActivity dials
        // mule's voice_call workflow directly — no ranch frames, no
        // daemon in the media path). Mule config lives in Settings
        // (next to the H6.2 forge fields). Sized as a real touch target
        // (≥44 dp), not an icon garnish — it's the primary action here.
        row.addView(Button(this).apply {
            text = "📞"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            minWidth = dp(48)
            minimumWidth = dp(48)
            minHeight = dp(40)
            setPadding(dp(10), 0, dp(10), 0)
            setOnClickListener {
                startActivity(Intent(this@AgentsActivity, CallActivity::class.java)
                    .putExtra(CallActivity.EXTRA_AGENT_ID, a.id)
                    .putExtra(CallActivity.EXTRA_AGENT_NAME, a.name))
            }
        })
        // H6.5b: the ✎ editor (forge PATCH /agents/:id + the agent's
        // mule wakes). The whole registry row rides along as the
        // prefill — no extra round trip before the form paints.
        row.addView(Button(this).apply {
            text = "✎"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7FD4FF.toInt())
            minWidth = dp(40)
            minimumWidth = dp(40)
            minHeight = dp(40)
            setPadding(dp(8), 0, dp(8), 0)
            setOnClickListener {
                startActivity(
                    Intent(this@AgentsActivity, EditAgentActivity::class.java)
                        .putExtra(EditAgentActivity.EXTRA_AGENT, agentRowJson(a).toString())
                )
            }
        })
        row.addView(Button(this).apply {
            text = "✕"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF6B7280.toInt())
            minWidth = dp(40)
            minimumWidth = dp(40)
            minHeight = dp(40)
            setPadding(dp(8), 0, dp(8), 0)
            setOnClickListener {
                android.app.AlertDialog.Builder(this@AgentsActivity)
                    .setTitle("Delete agent")
                    .setMessage("Delete \"${a.name.ifEmpty { a.id.take(8) }}\" from the registry? Its conversations and memory stay in forge but the agent row is removed.")
                    .setPositiveButton("delete") { _, _ ->
                        relay?.send(Term.agentDelete(a.id))
                        flash("deleting ${a.name.ifEmpty { a.id.take(8) }}…")
                    }
                    .setNegativeButton("cancel", null)
                    .show()
            }
        })
        val dot = TextView(this).apply {
            text = "○"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF4b5563.toInt())
            gravity = Gravity.CENTER_VERTICAL
        }
        agentDots[a.id] = dot
        row.addView(dot)
        return row
    }

    /** The row back as the wire JSON the editor parses (the same
     *  field names `AgentListOk` carries) — the prefill source. */
    private fun agentRowJson(a: Term.AgentInfo): JSONObject = JSONObject()
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
        }

    /** Deterministic colored circle with the agent's initial (avatar
     *  placeholder — no image loader in the native app yet). */
    private fun avatar(a: Term.AgentInfo): View {
        val c = (Math.abs((a.name + a.id).hashCode()) % 0xFFFFFF).toInt()
            .or(0xFF304050.toInt())
        val d = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(c)
        }
        return TextView(this).apply {
            text = (a.name.firstOrNull() ?: '?').toString().uppercase()
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(Color.WHITE)
            gravity = Gravity.CENTER
            background = d
            val size = dp(36)
            layoutParams = LinearLayout.LayoutParams(size, size)
        }
    }

    private fun abbrev(s: String): String =
        if (s.length <= 24) s else "…" + s.takeLast(24)

    private fun hint(msg: String): TextView = TextView(this).apply {
        text = msg; setTextColor(0xFF6b7280.toInt())
        setPadding(dp(10), dp(6), dp(10), dp(6))
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
    }

    // ---- local pi rows (Phase 4, unchanged) ----

    private fun piRow(s: Term.PiSession): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        val mark = if (s.active) "●" else "○"
        val ext = if (s.external) " [ext]" else ""
        wrap.addView(TextView(this).apply {
            text = "$mark ${s.title.ifEmpty { s.id.take(8) }}$ext"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(if (s.active) 0xFF7fd4ff.toInt() else 0xFFd1d5db.toInt())
        })
        wrap.addView(TextView(this).apply {
            text = s.path.ifEmpty { "?" }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6b7280.toInt())
        })
        wrap.addView(Button(this).apply {
            text = if (s.active) "open in ranch" else "adopt into ranch"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, dp(4), 0, dp(4))
            setOnClickListener {
                relay?.send(Term.adoptPiSession(s.sessionFile))
                status.text = "adopting ${s.title.ifEmpty { s.id.take(8) }}…"
            }
        })
        return wrap
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@AgentsActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@AgentsActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
