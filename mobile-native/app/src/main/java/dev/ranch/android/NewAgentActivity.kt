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
import android.widget.CheckBox
import android.widget.EditText
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
        /** The harness tool names the allowlist offers (agents_new.rs KNOWN_TOOLS). */
        private val KNOWN_TOOLS = listOf(
            "bash", "read", "write", "webfetch", "search",
            "web_login", "web_login_done", "spawn_subagent",
            "memory_remember", "agent_signal", "schedule_reminder",
        )
    }

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private var busy = false

    // profile picker state
    private val profiles = mutableListOf<Triple<String, String, String>>() // id, name, "provider/model"
    private var pickedProfile = ""
    private var profileChip: Button? = null
    private var modelCatalog: List<Triple<String, String, String>> = emptyList() // provider, id, name

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
            setOnClickListener { showProfilePicker() }
        }
        form.addView(profileChip)
        form.addView(Button(this).apply {
            text = "＋ new profile"
            setTextColor(0xFF7FD4FF.toInt())
            isAllCaps = false
            setPadding(0, dp(4), 0, 0)
            setOnClickListener { showNewProfileForm() }
        })
        val orgId = field(form, "org label (org scope only)", "org")
        val machine = field(form, "home machine (blank = this machine)", "")
        val wake = field(form, "wake time: HH:MM or 5-field cron (blank = none)", "07:00")
        val wakePrompt = field(form, "wake prompt (blank = default)", "")

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
        val toolsBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val toolChecks = mutableListOf<CheckBox>()
        for (t in KNOWN_TOOLS) {
            val box = CheckBox(this).apply {
                text = t
                setTextColor(0xFFE5E5E5.toInt())
                textSize = 13f
                setPadding(0, dp(2), 0, dp(2))
            }
            toolChecks.add(box)
            toolsBox.addView(box)
        }
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
                val tools = toolChecks.filter { it.isChecked }.map { it.text.toString() }
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
                val arr = f.optJSONArray("profiles") ?: return
                profiles.clear()
                for (i in 0 until arr.length()) {
                    val p = arr.getJSONObject(i)
                    val id = p.optString("id")
                    if (id.isEmpty()) continue
                    val pm = "${p.optString("provider")}/${p.optString("model")}"
                    profiles.add(Triple(id, p.optString("name", id), pm))
                }
                paintProfileChip()
            }
            "ModelCatalogOk" -> {
                val arr = f.optJSONArray("models") ?: return
                modelCatalog = (0 until arr.length()).map { i ->
                    val m = arr.getJSONObject(i)
                    Triple(m.optString("provider"), m.optString("id"), m.optString("name"))
                }
            }
            "AgentProvisionOk" -> {
                busy = false
                val n = f.optString("agent_name")
                val wake = f.optString("wake_id", "")
                flash(
                    "created $n" +
                        (if (wake.isNotEmpty()) " + wake ${wake.take(8)}" else ""),
                    green = true,
                )
                handler.postDelayed({ finish() }, 900)
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
        chip.text = when {
            pickedProfile.isNotEmpty() -> {
                val p = profiles.firstOrNull { it.first == pickedProfile }
                "profile: ${p?.second ?: pickedProfile.take(8)}${if (p != null) " (${p.third})" else ""}"
            }
            profiles.isEmpty() -> "no profiles — forge unreachable? (use ＋ new profile)"
            else -> "profile: none (blank agent)"
        }
    }

    /** The profile picker: one row per profile + "none". */
    private fun showProfilePicker() {
        if (profiles.isEmpty()) {
            flash("no profiles loaded (forge unreachable?) — use ＋ new profile")
            return
        }
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        val dialog = android.app.AlertDialog.Builder(this)
            .setTitle("Choose the agent's model profile")
            .setView(body)
            .create()
        fun addRow(label: String, sub: String, pick: String) {
            val row = Button(this).apply {
                text = label + (if (sub.isNotEmpty()) "\n$sub" else "")
                isAllCaps = false
                gravity = Gravity.START or Gravity.CENTER_VERTICAL
                setTextColor(0xFFE5E5E5.toInt())
                setPadding(dp(10), dp(8), dp(10), dp(8))
                setOnClickListener {
                    pickedProfile = pick
                    paintProfileChip()
                    dialog.dismiss()
                }
            }
            body.addView(row)
        }
        addRow("no profile", "the agent has no model until one is attached", "")
        for (p in profiles) addRow(p.second, p.third, p.first)
        dialog.show()
    }

    /** Inline new-profile form (ProfilePut). Model rows come from the
     *  catalog; a custom model id works too when the catalog is empty. */
    private fun showNewProfileForm() {
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        fun addField(label: String, hint: String, multiline: Boolean = false): EditText {
            body.addView(TextView(this).apply {
                text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(0xFF9aa0a6.toInt())
            })
            val edit = EditText(this).apply {
                this.hint = hint
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFE5E5E5.toInt())
                setHintTextColor(0xFF4b5563.toInt())
                if (multiline) { minLines = 3; setSingleLine(false) } else setSingleLine(true)
                background.setTint(0xFF9aa0a6.toInt())
            }
            body.addView(edit)
            return edit
        }
        val pName = addField("name", "e.g. \"spark work\"")
        val provider = addField("provider", "proxy / openai / anthropic / …")
        val modelEdit = addField("model", "provider/model (or see model below)")
        val baseUrl = addField("base url (blank = provider default)", "http://…")
        val apiKey = addField("api key (blank = use forge's stored key)", "sk_…")
        val workDir = addField("working dir (blank = none)", "/root/…")
        val prompt = addField("system prompt (blank = none)", "You are …", multiline = true)

        if (modelCatalog.isNotEmpty()) {
            body.addView(TextView(this).apply {
                text = "or pick a catalog model:"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, 0)
            })
            val pick = ScrollView(this)
            val inner = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
            for (m in modelCatalog) {
                val b = Button(this).apply {
                    text = "${m.second}"
                    isAllCaps = false
                    setTextColor(0xFFE5E5E5.toInt())
                    setPadding(0, dp(2), 0, dp(2))
                    setOnClickListener {
                        provider.setText(m.first)
                        modelEdit.setText("${m.first}/${m.second}")
                    }
                }
                inner.addView(b, LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT,
                    LinearLayout.LayoutParams.WRAP_CONTENT
                ).apply { setMargins(0, 0, dp(6), 0) })
            }
            pick.addView(inner)
            body.addView(pick, LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, dp(160)
            ))
        }

        val dialog = android.app.AlertDialog.Builder(this)
            .setTitle("New forge profile")
            .setView(body)
            .setPositiveButton("save profile") { _, _ ->
                val n = pName.text.toString().trim()
                val prov = provider.text.toString().trim()
                val md = modelEdit.text.toString().trim()
                if (n.isEmpty() || prov.isEmpty() || md.isEmpty()) {
                    android.widget.Toast.makeText(this, "name, provider and model are required", 0).show()
                    return@setPositiveButton
                }
                relay?.send(
                    Term.profilePut(
                        name = n,
                        provider = prov,
                        model = md,
                        baseUrl = baseUrl.text.toString().trim(),
                        apiKey = apiKey.text.toString().trim(),
                        workingDir = workDir.text.toString().trim(),
                        systemPrompt = prompt.text.toString().trim(),
                    )
                )
            }
            .setNegativeButton("cancel", null)
            .create()
        dialog.show()
    }

    private fun field(parent: LinearLayout, label: String, hint: String): EditText {
        parent.addView(TextView(this).apply {
            text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(8), 0, dp(2))
        })
        val edit = EditText(this).apply {
            this.hint = hint
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
            setHintTextColor(0xFF4b5563.toInt())
            setSingleLine(true)
            background.setTint(0xFF9aa0a6.toInt())
        }
        parent.addView(edit)
        return edit
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
