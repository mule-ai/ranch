package dev.ranch.android

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject

/**
 * Herd H5.3 — the Activity View: the herd's live timeline.
 *
 * Data is push-only: the daemon's activity poller recomputes the full
 * item set every ~5 s and broadcasts it as `ActivityPush`; this screen
 * just replaces its in-memory timeline with each new set (no polling,
 * no persistence in v1). Items are grouped by agent — machine-wide
 * items (local ask/spawn/trigger/mule_job, `agent: null`) group under
 * "local" — groups ordered by their most-recent item, items
 * newest-first; each row is "glyph text · HH:MM" (the TUI's H5.3
 * glyph map: 🧠 belief, ✅ ask, 🔬 research, ⏰ wake, ▸ forge task,
 * ⇄ mule job, ⚡ trigger, ✚ spawn, · unknown).
 *
 * The header "Pause herd"/"Resume herd" button sends
 * `AgentPause { agent: null, paused: <toggle> }` — the global kill
 * switch (forge pause + mule wake-pause, per-agent in the reply).
 * Long-pressing an item offers "Pause agent <name>"/"Resume agent
 * <name>" for that item's agent. Both toggle off the last CONFIRMED
 * target (`pausedHint`, "herd" or an agent name — a failed
 * `AgentPauseOk` leg leaves it unknown). `AgentPauseOk` toasts the
 * per-agent results; `Error` replies flash the status line.
 *
 * Tapping an item deep-links to that agent — the same
 * `AgentTalk{mode:"continue"}` path as [AgentsActivity]; machine-wide
 * items have no agent to open in v1 (flash instead).
 */
class HerdActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private lateinit var pauseBtn: Button
    private lateinit var timeline: LinearLayout

    /** Latest `ActivityPush` set (most-recent-first); replaced whole. */
    private val items = ArrayList<Term.ActivityItem>()
    /** Cached agent registry — deep links need name -> id. */
    private val agents = ArrayList<Term.AgentInfo>()
    /** Last CONFIRMED pause target: "herd" or an agent registry name. */
    private var pausedHint: String? = null
    /** (target null = herd, requested state) awaiting AgentPauseOk. */
    private var pendingPause: Pair<String?, Boolean>? = null
    /** AgentTalk req_id -> agent name while a deep link is in flight. */
    private val pendingTalk = HashMap<String, String>()

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
        bar.addView(Button(this).apply { text = "←"; setOnClickListener { finish() } })
        bar.addView(TextView(this).apply {
            text = "Activity"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, 0, 1f))
        pauseBtn = Button(this).apply {
            text = "Pause herd"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setOnClickListener { pauseHerd() }
        }
        bar.addView(pauseBtn)
        root.addView(bar)

        status = TextView(this).apply {
            text = "waiting for activity…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        timeline = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(timeline) })

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.agentList()) // deep links resolve name -> id from this
        render()
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "ActivityPush" -> {
                items.clear()
                f.optJSONArray("items")?.let { a ->
                    for (i in 0 until a.length())
                        items.add(Term.parseActivityItem(a.getJSONObject(i)))
                }
                status.text = "live · ${items.size} item(s)"
                status.setTextColor(0xFF4ade80.toInt())
                render()
            }
            "AgentListOk" -> {
                agents.clear()
                f.optJSONArray("agents")?.let { a ->
                    for (i in 0 until a.length())
                        agents.add(Term.parseAgentInfo(a.getJSONObject(i)))
                }
            }
            "AgentTalkOk" -> {
                val name = pendingTalk.remove(f.optString("req_id")) ?: return
                startActivity(Intent(this, SessionActivity::class.java)
                    .putExtra("sessionId", f.optString("session"))
                    .putExtra("sessionName", name))
            }
            "AgentPauseOk" -> {
                val results = ArrayList<Term.AgentPauseResult>()
                f.optJSONArray("results")?.let { a ->
                    for (i in 0 until a.length())
                        results.add(Term.parseAgentPauseResult(a.getJSONObject(i)))
                }
                Toast.makeText(this, "pause: " + pauseStatus(results), Toast.LENGTH_LONG)
                    .show()
                pendingPause?.let { (target, paused) ->
                    if (results.all { it.error == null }) {
                        pausedHint = target ?: "herd" // clean answer -> set the hint
                        render()
                    }
                }
                pendingPause = null
            }
            "Error" -> {
                if (f.optString("req_id").startsWith("ap-")) {
                    pendingPause = null
                    flash("pause: ${f.optString("message", "error")}")
                }
            }
        }
    }

    // ---- pause (H5.3 kill switch) ----

    /** Herd-level toggle: target "herd" unless we last confirmed the
     *  herd paused, in which case this resumes it. */
    private fun pauseHerd() {
        val paused = pausedHint != "herd"
        pendingPause = null to paused
        relay?.send(Term.agentPause(null, paused))
    }

    /** Item-level toggle: long-press on an agent's item. */
    private fun pauseAgent(name: String) {
        val paused = pausedHint != name
        val frame = Term.agentPause(name, paused)
        pendingPause = name to paused
        relay?.send(frame)
    }

    /** One status line for `AgentPauseOk`: ok/failed counts + the
     *  first failure's detail (mirrors the TUI's `activity_pause_status`). */
    private fun pauseStatus(results: List<Term.AgentPauseResult>): String {
        if (results.isEmpty()) return "no agents known"
        val failed = results.count { it.error != null }
        val base = if (failed == 0) "${results.size} agent(s) ok"
        else "${results.size - failed} ok, $failed failed"
        results.firstOrNull { it.error != null }?.let {
            return "$base — ${it.name}: ${it.error ?: "unknown error"}"
        }
        return base
    }

    private fun flash(msg: String) {
        status.text = msg
        status.setTextColor(0xFFF59E0B.toInt())
        handler.postDelayed({ status.setTextColor(0xFF9aa0a6.toInt()) }, 4000)
    }

    // ---- timeline ----

    private fun glyph(kind: String): String = when (kind) {
        "belief_pending" -> "🧠"
        "ask" -> "✅"
        "research_open" -> "🔬"
        "wake" -> "⏰"
        "forge_task" -> "▸"
        "mule_job" -> "⇄"
        "trigger" -> "⚡"
        "spawn" -> "✚"
        else -> "·"
    }

    private fun hhmm(ts: String): String =
        if (ts.length >= 16 && ts[10] == 'T') ts.substring(11, 16) else ""

    /** One group per agent ("local" for machine-wide items), groups
     *  ordered by their most-recent item, items newest-first — the TUI's
     *  `activity_groups` (RFC3339-UTC strings sort lexicographically). */
    private fun groups(): List<Pair<String, List<Int>>> {
        val order = (items.indices.toList())
            .sortedByDescending { items[it].ts }
        val out = ArrayList<Pair<String, MutableList<Int>>>()
        for (i in order) {
            val key = items[i].agent ?: "local"
            val g = out.firstOrNull { it.first == key }
            if (g != null) g.second.add(i) else out.add(key to arrayListOf(i))
        }
        return out.map { it.first to it.second }
    }

    private fun render() {
        pauseBtn.text = if (pausedHint == "herd") "Resume herd" else "Pause herd"
        timeline.removeAllViews()
        if (items.isEmpty()) {
            timeline.addView(hint("No activity."))
            return
        }
        for ((group, idxs) in groups()) {
            timeline.addView(TextView(this).apply {
                text = if (group == "local") "LOCAL" else group.uppercase()
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF4ade80.toInt())
                setPadding(0, dp(10), 0, dp(2))
            })
            for (i in idxs) {
                timeline.addView(itemRow(items[i]))
            }
        }
    }

    private fun itemRow(item: Term.ActivityItem): LinearLayout {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(10), dp(8), dp(10), dp(8))
            setOnClickListener { openItem(item) }
            setOnLongClickListener {
                when (val name = item.agent) {
                    null ->
                        Toast.makeText(this@HerdActivity,
                            "machine-local item — no agent to pause", Toast.LENGTH_SHORT).show()
                    else ->
                        AlertDialog.Builder(this@HerdActivity)
                            .setTitle(name)
                            .setPositiveButton(
                                if (pausedHint == name) "Resume agent" else "Pause agent") { _, _ ->
                                pauseAgent(name)
                            }
                            .setNegativeButton("cancel", null)
                            .show()
                }
                true
            }
        }
        val text = StringBuilder(glyph(item.kind)).append(' ')
            .append(item.text)
            .append(if (item.agent == null) "" else "  ·  ${item.agent}")
        hhmm(item.ts).takeIf { it.isNotEmpty() }?.let {
            text.append("  ").append(it)
        }
        row.addView(TextView(this).apply {
            this.text = text.toString()
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFd1d5db.toInt())
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        return row
    }

    /** Deep link: open the item's agent's most-active conversation —
     *  the H1.4 `AgentTalk{continue}` path (name -> id from the cached
     *  registry). Machine-wide items flash instead (v1: no target). */
    private fun openItem(item: Term.ActivityItem) {
        val name = item.agent ?: run {
            flash(if (item.kind == "ask")
                "pending approval — open it from the sessions list (v1: no cross-pane focus)"
                else "machine-local event — open it from the sessions list")
            return
        }
        val a = agents.firstOrNull { it.name == name } ?: run {
            flash("agent '$name' not in the registry yet — retry in a moment")
            return
        }
        val frame = Term.agentTalk(a.id, "continue")
        pendingTalk[frame.getString("req_id")] = name
        relay?.send(frame)
        flash("opening $name…")
    }

    private fun hint(msg: String): TextView = TextView(this).apply {
        text = msg; setTextColor(0xFF6b7280.toInt())
        setPadding(dp(10), dp(12), dp(10), dp(12))
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@HerdActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@HerdActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
