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
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject

/**
 * Herd H6.5 v2 — the mobile "＋ Agent" builder (the [AgentsActivity]
 * wizard twin; same core as `ranch agents new`: forge `POST /agents`
 * + optional mule cron wake, via `AgentProvision`).
 *
 * v2 adds the CLI's richer half that v1 skipped (see design/agent-builder.md
 * §7.4 "v1 limits"): a real **profile picker** (`ProfileList` — name +
 * provider/model per row) with an inline **new-profile** form
 * (`ModelCatalog` for the model picker, `ProfilePut` to create), and a
 * **tools allowlist** checkbox set (the daemon's KNOWN_TOOLS; unchecked
 * = all tools, the forge/H1.1 convention of an empty allowlist).
 *
 * What this creates vs. the other two agent-ish things (kept distinct
 * on purpose): a **herd agent** — a registry row with a name, model
 * (via its profile), memory scope and wakes. One-off chats (pi on the
 * machine / lab forge profile) are disposable sessions created from
 * the monitor home; they are NOT registry entries.
 *
 * `AgentProvisionOk` → green flash + finish. `Error` (rid "apv-") →
 * amber flash, form stays (flash, don't die — M8.4).
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

    // profile picker state (shared shape with the ✎ editor — AgentForm)
    private val profiles = mutableListOf<AgentForm.ProfileRow>()
    private var pickedProfile = ""
    private var profileChip: Button? = null
    private var modelCatalog: List<AgentForm.ModelRow> = emptyList()

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
        val name = AgentForm.field(this, form, "name", "e.g. Atlas")
        val avatar = AgentForm.field(this, form, "avatar (url, blank = none)", "https://…")

        // ---- model profile (picker + inline new-profile) ----
        form.addView(TextView(this).apply {
            text = "model profile"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(8), 0, dp(2))
        })
        profileChip = Button(this).apply {
            text = "loading profiles…"
            setTextColor(0xFFE5E5E5.toInt())
            isAllCaps = false
            setPadding(dp(10), dp(8), dp(10), dp(8))
            setOnClickListener {
                if (profiles.isEmpty()) {
                    flash("no profiles loaded (forge unreachable?) — use ＋ new profile")
                    return@setOnClickListener
                }
                AgentForm.showProfilePicker(this@NewAgentActivity, profiles, pickedProfile) { pick ->
                    pickedProfile = pick
                    paintProfileChip()
                }
            }
        }
        form.addView(profileChip)
        form.addView(Button(this).apply {
            text = "＋ new profile"
            setTextColor(0xFF7FD4FF.toInt())
            isAllCaps = false
            setPadding(0, dp(4), 0, 0)
            setOnClickListener {
                AgentForm.showNewProfileForm(this@NewAgentActivity, modelCatalog) { frame ->
                    relay?.send(frame)
                }
            }
        })
        val orgId = AgentForm.field(this, form, "org label (org scope only)", "org")
        val machine = AgentForm.field(this, form, "home machine (blank = this machine)", "")
        val wake = AgentForm.field(this, form, "wake time: HH:MM or 5-field cron (blank = none)", "07:00")
        val wakePrompt = AgentForm.field(this, form, "wake prompt (blank = default)", "")

        // memory scope toggle: two small buttons
        val scopeRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val scopeAgent = Button(this).apply { text = "memory: agent"; setTextColor(Color.WHITE) }
        val scopeOrg = Button(this).apply { text = "memory: org" }
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

        // ---- tools allowlist (checkboxes; unchecked = all tools) ----
        form.addView(TextView(this).apply {
            text = "tools (check what this agent may use; none checked = all)"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(8), 0, dp(2))
        })
        val (toolsBox, toolsGet) = AgentForm.toolsBox(this, emptySet())
        form.addView(toolsBox)

        root.addView(ScrollView(this).apply { addView(form) },
            LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f
            ))

        val create = Button(this).apply {
            text = "Create agent"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                if (busy) return@setOnClickListener
                val n = name.text.toString().trim()
                if (n.isEmpty()) { flash("name is required"); return@setOnClickListener }
                if (n.length > 64) { flash("name too long (max 64)"); return@setOnClickListener }
                val tools = toolsGet()
                busy = true
                status.text = "creating $n…"
                relay?.send(
                    Term.agentProvision(
                        name = n,
                        avatar = avatar.text.toString().trim(),
                        memoryScope = scope,
                        orgId = if (scope == "org") orgId.text.toString().trim() else "",
                        homeMachine = machine.text.toString().trim(),
                        profileId = pickedProfile,
                        wakeCron = wake.text.toString().trim(),
                        wakePrompt = wakePrompt.text.toString().trim(),
                        tools = tools,
                    )
                )
            }
        }
        root.addView(create)

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.profileList())
        r.send(Term.modelCatalog())
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "ProfileListOk" -> {
                profiles.clear()
                profiles.addAll(AgentForm.parseProfiles(f.optJSONArray("profiles")))
                paintProfileChip()
            }
            "ModelCatalogOk" -> {
                modelCatalog = AgentForm.parseModels(f.optJSONArray("models"))
            }
            "AgentProvisionOk" -> {
                busy = false
                val n = f.optString("agent_name")
                val wake = f.optString("wake_id", "")
                val wakeErr = f.optString("wake_error", "")
                val msg = "created $n" +
                    (if (wake.isNotEmpty()) " + wake ${wake.take(8)}" else "") +
                    (if (wakeErr.isNotEmpty()) "\n⚠ wake: $wakeErr" else "")
                flash(msg, green = wakeErr.isEmpty())
                if (wakeErr.isEmpty()) handler.postDelayed({ finish() }, 900)
            }
            "ProfilePutOk" -> {
                pickedProfile = f.optString("profile_id")
                flash("profile saved — attached to this agent", green = true)
            }
            "Error" -> {
                when {
                    f.optString("req_id").startsWith("apv-") -> {
                        busy = false
                        flash("create failed: ${f.optString("message", "error")}")
                    }
                    f.optString("req_id").startsWith("pp-") ->
                        flash("profile save failed: ${f.optString("message", "error")}")
                }
            }
        }
    }

    private fun paintProfileChip() {
        val chip = profileChip ?: return
        AgentForm.paintChip(chip, pickedProfile, profiles)
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
