package dev.ranch.android

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject

/**
 * Herd H6.5 — the mobile "＋ Agent" form (the [AgentsActivity] wizard
 * twin). Collects the specialist-dot fields and sends
 * `AgentProvision` to the daemon, which runs the SAME core as
 * `ranch agents new`: forge `POST /agents` + optional mule cron wake.
 *
 * Fields (v1): name (required), avatar url, memory scope
 * (agent/org + org label), home machine (blank = the daemon's own
 * relay machine id), profile id (blank = none; the profile picker is
 * the CLI's richer half), wake time "HH:MM" or a 5-field cron
 * (blank = none), wake prompt (blank = default). Tools allowlist is
 * "all" here — restrict later via `PATCH /agents/:id`.
 *
 * `AgentProvisionOk` → green flash + finish (the registry screen
 * refreshes itself on the same frame). `Error` (rid "apv-") → amber
 * flash, form stays (flash, don't die — M8.4).
 */
class NewAgentActivity : Activity() {

    companion object {
        const val EXTRA_PRESET_NAME = "presetName"
    }

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private var busy = false

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
            text = "New agent"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }
        bar.addView(back)
        bar.addView(title, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = ""; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFFF59E0B.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        val form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(4), 0, dp(4))
        }
        val name = field(form, "name", "e.g. Atlas")
        val avatar = field(form, "avatar (url, blank = none)", "https://…")
        val orgId = field(form, "org label (org scope only)", "org")
        val machine = field(form, "home machine (blank = this machine)", "")
        val profile = field(form, "profile id (blank = none)", "")
        val wake = field(form, "wake time: HH:MM or 5-field cron (blank = none)", "07:00")
        val wakePrompt = field(form, "wake prompt (blank = default)", "")

        // memory scope toggle: two small buttons
        val scopeRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val scopeAgent = Button(this).apply {
            text = "memory: agent"; setTextColor(Color.WHITE)
        }
        val scopeOrg = Button(this).apply {
            text = "memory: org"
        }
        var scope = "agent"
        fun paint() {
            scopeAgent.setBackgroundColor(if (scope == "agent") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
            scopeOrg.setBackgroundColor(if (scope == "org") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
            orgId.isEnabled = scope == "org"
        }
        scopeAgent.setOnClickListener { scope = "agent"; paint() }
        scopeOrg.setOnClickListener { scope = "org"; paint() }
        paint()
        scopeRow.addView(scopeAgent, LinearLayout.LayoutParams(0, 0, 1f))
        scopeRow.addView(scopeOrg, LinearLayout.LayoutParams(0, 0, 1f))
        form.addView(scopeRow)

        root.addView(ScrollView(this).apply { addView(form) })

        val create = Button(this).apply {
            text = "Create agent"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                if (busy) return@setOnClickListener
                val n = name.text.toString().trim()
                if (n.isEmpty()) { flash("name is required"); return@setOnClickListener }
                if (n.length > 64) { flash("name too long (max 64)"); return@setOnClickListener }
                busy = true
                status.text = "creating {n}…"
                relay?.send(
                    Term.agentProvision(
                        name = n,
                        avatar = avatar.text.toString().trim(),
                        memoryScope = scope,
                        orgId = if (scope == "org") orgId.text.toString().trim() else "",
                        homeMachine = machine.text.toString().trim(),
                        profileId = profile.text.toString().trim(),
                        wakeCron = wake.text.toString().trim(),
                        wakePrompt = wakePrompt.text.toString().trim(),
                    )
                )
            }
        }
        root.addView(create)

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "AgentProvisionOk" -> {
                busy = false
                val name = f.optString("agent_name")
                val wake = f.optString("wake_id", "")
                flash(
                    "created {name}" +
                        (if (wake.isNotEmpty()) " + wake ${wake.take(8)}" else ""),
                    green = true,
                )
                // The registry screen (same sink network) refreshes on
                // this frame too; head back to it.
                handler.postDelayed({ finish() }, 900)
            }
            "Error" -> {
                if (f.optString("req_id").startsWith("apv-")) {
                    busy = false
                    flash("create failed: ${f.optString("message", "error")}")
                }
            }
        }
    }

    private fun field(parent: LinearLayout, label: String, hint: String): EditText {
        parent.addView(TextView(this).apply {
            text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(8), 0, dp(2))
        })
        return EditText(this).apply {
            this.hint = hint
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
            setHintTextColor(0xFF4b5563.toInt())
            setSingleLine(true)
            background.setTint(0xFF9aa0a6.toInt())
        }
    }

    private fun flash(msg: String, green: Boolean = false) {
        status.text = msg
        status.setTextColor(if (green) 0xFF34d399.toInt() else 0xFFF59E0B.toInt())
        if (!green) handler.postDelayed({ status.text = "" }, 6000)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@NewAgentActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@NewAgentActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
