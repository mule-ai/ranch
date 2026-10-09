package dev.ranch.android

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import android.view.View
import org.json.JSONObject

/**
 * H6.1 — per-agent notification settings: mute + priority
 * (high / normal / quiet) for each agent in the forge registry.
 *
 * Semantics (see [NotifyPolicy]):
 *  - muted     the agent produces NO notifications — its items still
 *              appear in the Activity View (mute is notification-only)
 *  - quiet     items still notify, on the low-importance "ranch.quiet"
 *              channel (no sound/vibration)
 *  - high      items notify on the high-importance "ranch.high" channel
 *  - normal    the default "ranch" channel
 *
 * Persisted as the `agent_notify` JSON pref
 * ({agentName: {muted, priority}}) — shared with the notification
 * engine via [Avatars.parseAgentPrefs].
 */
class NotifyAgentsActivity : Activity() {

    private val app get() = application as App
    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var list: LinearLayout
    private lateinit var status: TextView

    /** agentName -> its row controls, rebuilt on each AgentListOk. */
    private val rows = HashMap<String, Row>()

    private class Row(val switch: Switch, val buttons: Array<Button>)

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
            text = "Agent notifications"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, 0, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "loading agents…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(list) })

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.agentList())
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "AgentListOk" -> {
                val agents = mutableListOf<Term.AgentInfo>()
                f.optJSONArray("agents")?.let { a ->
                    for (i in 0 until a.length()) agents.add(Term.parseAgentInfo(a.getJSONObject(i)))
                }
                status.text = if (agents.isEmpty()) "no agents registered"
                else "${agents.size} agent(s) — toggle mute, pick priority"
                renderAgents(agents)
            }
            "Error" -> {
                val rid = f.optString("req_id")
                if (rid.startsWith("al-")) status.text = "registry: ${f.optString("message", "error")}"
            }
        }
    }

    private fun renderAgents(agents: List<Term.AgentInfo>) {
        list.removeAllViews()
        rows.clear()
        val prefs = Avatars.parseAgentPrefs(app.prefs.getJson("agent_notify"))
        for (a in agents.sortedBy { it.name.lowercase() }) {
            val cur = prefs[a.name] ?: NotifyPolicy.AgentPrefs()
            list.addView(agentRow(a.name, cur))
        }
    }

    private fun agentRow(name: String, cur: NotifyPolicy.AgentPrefs): LinearLayout {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF1B2126.toInt())
            setPadding(dp(10), dp(8), dp(10), dp(8))
        }
        val top = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        top.addView(TextView(this).apply {
            text = name; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
        }, LinearLayout.LayoutParams(0, 0, 1f))
        val mute = Switch(this).apply {
            text = "mute"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            isChecked = cur.muted
            setOnCheckedChangeListener { _, v -> save(name, v, cur.priority) }
        }
        top.addView(mute)
        row.addView(top)

        val prioRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, dp(6), 0, 0)
        }
        prioRow.addView(TextView(this).apply {
            text = "priority:"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); gravity = Gravity.CENTER_VERTICAL
        }, LinearLayout.LayoutParams(0, 0, 1.2f))
        val buttons = Array(3) { i ->
            Button(this).apply {
                text = PRIORITY_LABELS[i]
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setPadding(dp(8), 0, dp(8), 0)
                minWidth = 0; minimumWidth = 0
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
                setOnClickListener { save(name, mute.isChecked, PRIORITY_VALUES[i]) }
            }
        }
        for (b in buttons) prioRow.addView(b)
        paintPriority(buttons, cur.priority)
        row.addView(prioRow)

        rows[name] = Row(mute, buttons)
        return row
    }

    /** Highlight the active priority button. */
    private fun paintPriority(buttons: Array<Button>, priority: String) {
        val idx = PRIORITY_VALUES.indexOf(priority).coerceAtLeast(1)
        buttons.forEachIndexed { i, b ->
            b.setBackgroundColor(if (i == idx) 0xFF2D5A3D.toInt() else 0xFF262C33.toInt())
            b.setTextColor(if (i == idx) 0xFF8FE3B4.toInt() else 0xFFC9CDD3.toInt())
        }
    }

    private fun save(name: String, muted: Boolean, priority: String) {
        val all = Avatars.parseAgentPrefs(app.prefs.getJson("agent_notify")).toMutableMap()
        all[name] = NotifyPolicy.AgentPrefs(muted, priority)
        val o = JSONObject()
        for ((k, v) in all) o.put(k, JSONObject().put("muted", v.muted).put("priority", v.priority))
        app.prefs.setJson("agent_notify", o.toString())
        status.text = "$name: " +
            (if (muted) "muted — Activity View still shows its items" else "on") +
            " · $priority"
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@NotifyAgentsActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@NotifyAgentsActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }

    companion object {
        val PRIORITY_VALUES = arrayOf("high", "normal", "quiet")
        val PRIORITY_LABELS = arrayOf("high", "normal", "quiet")
    }
}
