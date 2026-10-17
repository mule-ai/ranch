package dev.ranch.android

import android.app.Activity
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
 * Herd H6.5b — the ✎ herd **agent editor** (the mobile twin of
 * `ranch agents edit`; same core server-side: forge
 * `PATCH /agents/:id` via the `AgentUpdate` frame, mule wakes via the
 * `WakeList`/`WakePut`/`WakeDelete` frames).
 *
 * Opens from an [AgentsActivity] registry row. Every control starts at
 * the agent's CURRENT value (the row the registry list already
 * carries — `AgentListOk` now ships profile / memory scope / org /
 * visibility / tools / extra instructions / paused), and Save sends
 * ONLY what changed: absent means "leave the column alone" in forge's
 * partial update, so untouched fields are simply not on the wire.
 *
 * Two sections:
 *  - **agent row** — name, avatar, model profile (the [AgentForm]
 *    picker shared with the ＋Agent builder, incl. the inline
 *    `ProfilePut` new-profile form), home machine, memory scope
 *    (+ org label), visibility, tools allowlist, extra instructions.
 *  - **wakes** — the agent's mule cron wakes (`WakeList` filtered by
 *    agent): edit schedule/prompt, pause/resume, delete, add. A wake
 *    is a mule row, not an agent column, so it saves immediately
 *    rather than waiting for Save.
 *
 * Clearing a nullable column is NOT possible (forge treats JSON null
 * and an absent key identically — documented on the `AgentUpdate`
 * frame), so blanks here mean "keep", never "unset".
 *
 * Flash, don't die (M8.4): every `Error` reply names its req-id leg on
 * the status line; nothing here exits the screen.
 */
class EditAgentActivity : Activity() {

    companion object {
        const val EXTRA_AGENT = "agent"      // JSONObject string (Term.AgentInfo wire row)
        const val EXTRA_AGENT_ID = "agentId" // fallback: re-read from the registry list
    }

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private lateinit var form: LinearLayout
    private lateinit var wakeBox: LinearLayout
    private lateinit var saveBtn: Button
    private lateinit var profileChip: Button

    private var agent = Term.AgentInfo(id = "", name = "", avatarUrl = null, homeMachine = null,
        lastActive = null, conversationCount = null)
    private val profiles = mutableListOf<AgentForm.ProfileRow>()
    private var modelCatalog: List<AgentForm.ModelRow> = emptyList()
    private var pickedProfile = ""
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
        bar.addView(Button(this).apply { text = "←"; setOnClickListener { finish() } })
        bar.addView(TextView(this).apply {
            text = "Edit agent"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = ""; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFFF59E0B.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        // the row the registry screen already had; re-read on demand
        val raw = intent.getStringExtra(EXTRA_AGENT)
        if (raw != null) {
            runCatching { agent = Term.parseAgentInfo(JSONObject(raw)) }
        } else {
            intent.getStringExtra(EXTRA_AGENT_ID)?.let { agent = agent.copy(id = it) }
        }
        if (agent.id.isEmpty()) { setContentView(errorView("No agent selected.")); return }
        pickedProfile = agent.primaryProfile.orEmpty()

        // ---- agent-row fields (Save = diff) ----
        form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(4), 0, dp(4))
        }
        val name = AgentForm.field(this, form, "name", agent.name.ifEmpty { "e.g. Atlas" },
            initial = agent.name)
        val avatar = AgentForm.field(this, form, "avatar (url)", "https://…",
            initial = agent.avatarUrl.orEmpty())
        form.addView(AgentForm.label(this, "model profile"))
        profileChip = Button(this).apply {
            isAllCaps = false
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(dp(10), dp(8), dp(10), dp(8))
            setOnClickListener {
                if (profiles.isEmpty()) {
                    flash("no profiles loaded (forge unreachable?) — use ＋ new profile")
                    return@setOnClickListener
                }
                AgentForm.showProfilePicker(this@EditAgentActivity, profiles, pickedProfile) { pick ->
                    pickedProfile = pick
                    paintChip()
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
                AgentForm.showNewProfileForm(this@EditAgentActivity, modelCatalog) { frame ->
                    relay?.send(frame)
                }
            }
        })
        val machine = AgentForm.field(this, form, "home machine", "this machine's id",
            initial = agent.homeMachine.orEmpty())
        val orgId = AgentForm.field(this, form, "org label (org scope only)", "org",
            initial = agent.orgId.orEmpty())

        var scope = agent.memoryScope ?: "agent"
        val scopeRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val scopeAgent = Button(this).apply { text = "memory: agent" }
        val scopeOrg = Button(this).apply { text = "memory: org" }
        fun paintScope() {
            scopeAgent.setBackgroundColor(if (scope == "agent") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
            scopeOrg.setBackgroundColor(if (scope == "org") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
            orgId.isEnabled = scope == "org"
        }
        scopeAgent.setOnClickListener { scope = "agent"; paintScope() }
        scopeOrg.setOnClickListener { scope = "org"; paintScope() }
        paintScope()
        scopeRow.addView(scopeAgent, LinearLayout.LayoutParams(0, 0, 1f))
        scopeRow.addView(scopeOrg, LinearLayout.LayoutParams(0, 0, 1f))
        form.addView(scopeRow)

        var visibility = agent.visibility ?: "private"
        val visRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val visPrivate = Button(this).apply { text = "visibility: private" }
        val visOrg = Button(this).apply { text = "visibility: org" }
        fun paintVis() {
            visPrivate.setBackgroundColor(if (visibility == "private") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
            visOrg.setBackgroundColor(if (visibility == "org") 0xFF2563eb.toInt() else 0xFF2a2f36.toInt())
        }
        visPrivate.setOnClickListener { visibility = "private"; paintVis() }
        visOrg.setOnClickListener { visibility = "org"; paintVis() }
        paintVis()
        visRow.addView(visPrivate, LinearLayout.LayoutParams(0, 0, 1f))
        visRow.addView(visOrg, LinearLayout.LayoutParams(0, 0, 1f))
        form.addView(visRow)

        form.addView(AgentForm.label(this, "tools (check what this agent may use; none checked = all)"))
        val currentTools = agent.toolsAllowlist?.toSet() ?: emptySet()
        val (toolsBox, toolsGet) = AgentForm.toolsBox(this, currentTools)
        form.addView(toolsBox)
        form.addView(TextView(this).apply {
            text = "(unchanged: every tool the agent's profile offers)"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
            setTextColor(0xFF6b7280.toInt())
        })
        val instr = AgentForm.field(this, form, "extra instructions",
            "appended to the agent's system prompt", multiline = true,
            initial = agent.extraInstructions.orEmpty())

        // ---- wakes (saved per-row, not by the Save button) ----
        root.addView(ScrollView(this).apply {
            addView(LinearLayout(this@EditAgentActivity).apply {
                orientation = LinearLayout.VERTICAL
                addView(form)
                addView(AgentForm.label(this@EditAgentActivity, "wakes (mule cron)", 14))
                wakeBox = LinearLayout(this@EditAgentActivity).apply {
                    orientation = LinearLayout.VERTICAL
                }
                addView(wakeBox)
                addView(Button(this@EditAgentActivity).apply {
                    text = "＋ add wake"
                    isAllCaps = false
                    setTextColor(0xFF7FD4FF.toInt())
                    setPadding(0, dp(4), 0, dp(4))
                    setOnClickListener { showWakeForm(null) }
                })
            })
        }, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f
        ))

        saveBtn = Button(this).apply {
            text = "Save"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener { save(name, avatar, machine, orgId, scope, visibility, toolsGet, instr) }
        }
        root.addView(saveBtn)

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        paintChip()
        status.text = "editing \"${agent.name.ifEmpty { agent.id.take(8) }}\"" +
            (if (agent.paused == true) " · PAUSED" else "")

        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.profileList())
        r.send(Term.modelCatalog())
        r.send(Term.agentList())        // refresh our row (paused state, columns)
        r.send(Term.wakeList(agent.id))
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    /** Send ONLY what changed — absent = keep (forge null ≡ absent). */
    private fun save(
        name: EditText,
        avatar: EditText,
        machine: EditText,
        orgId: EditText,
        scope: String,
        visibility: String,
        toolsGet: () -> List<String>,
        instr: EditText,
    ) {
        if (busy) return
        val n = name.text.toString().trim()
        if (n.isEmpty()) { flash("name must not be empty"); return }
        if (n.length > 64) { flash("name too long (max 64)"); return }

        fun changed(e: EditText, current: String?): String? =
            e.text.toString().trim().takeIf { it.isNotEmpty() && it != (current ?: "").trim() }

        val tools = toolsGet()
        // an allowlist "change" only counts when the set differs from
        // the current one (empty set = all tools = empty allowlist)
        val currentTools = agent.toolsAllowlist ?: emptyList()
        val toolsPatch = tools.takeIf { it.sorted() != currentTools.sorted() }

        val frame = Term.agentUpdate(
            agentId = agent.id,
            name = n.takeIf { it != agent.name },
            avatar = changed(avatar, agent.avatarUrl),
            profileId = pickedProfile.takeIf { it.isNotEmpty() && it != (agent.primaryProfile ?: "") },
            memoryScope = scope.takeIf { it != (agent.memoryScope ?: "agent") },
            orgId = changed(orgId, agent.orgId),
            homeMachine = changed(machine, agent.homeMachine),
            visibility = visibility.takeIf { it != (agent.visibility ?: "private") },
            tools = toolsPatch,
            extraInstructions = changed(instr, agent.extraInstructions),
        )
        val keys = frame.keys().asSequence().filter { it !in setOf("t", "id", "client", "req_id", "agent_id") }
        if (!keys.iterator().hasNext()) { flash("nothing to save"); return }
        busy = true
        status.text = "saving…"
        relay?.send(frame)
    }

    // ---- frames ----

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "ProfileListOk" -> {
                profiles.clear()
                profiles.addAll(AgentForm.parseProfiles(f.optJSONArray("profiles")))
                paintChip()
            }
            "ModelCatalogOk" -> modelCatalog = AgentForm.parseModels(f.optJSONArray("models"))
            "ProfilePutOk" -> {
                // a profile created from here is meant for THIS agent
                pickedProfile = f.optString("profile_id")
                paintChip()
                flash("profile saved — hit Save to attach it", green = true)
            }
            "AgentUpdateOk" -> {
                busy = false
                f.optJSONObject("agent")?.let { agent = Term.parseAgentInfo(it) }
                pickedProfile = agent.primaryProfile.orEmpty()
                paintChip()
                flash("saved \"${agent.name}\"", green = true)
                relay?.send(Term.agentList())
            }
            "AgentListOk" -> {
                // re-read our own row (paused state / columns the tap
                // row passed before they existed)
                val arr = f.optJSONArray("agents") ?: return
                for (i in 0 until arr.length()) {
                    val o = arr.optJSONObject(i) ?: continue
                    val a = Term.parseAgentInfo(o)
                    if (a.id == agent.id) {
                        agent = a
                        pickedProfile = a.primaryProfile.orEmpty()
                        paintChip()
                        status.text = "editing \"${a.name.ifEmpty { a.id.take(8) }}\"" +
                            (if (a.paused == true) " · PAUSED" else "")
                        break
                    }
                }
            }
            "WakeListOk" -> renderWakes(
                (f.optJSONArray("wakes")?.let { arr ->
                    (0 until arr.length()).mapNotNull { i ->
                        arr.optJSONObject(i)?.let { Term.parseWakeInfo(it) }
                    }
                } ?: emptyList())
            )
            "WakePutOk" -> {
                flash("wake saved", green = true)
                relay?.send(Term.wakeList(agent.id))
            }
            "WakeDeleteOk" -> {
                flash("wake deleted", green = true)
                relay?.send(Term.wakeList(agent.id))
            }
            "Error" -> {
                val rid = f.optString("req_id")
                val msg = f.optString("message", "error")
                when {
                    rid.startsWith("au-") -> { busy = false; flash("save failed: $msg") }
                    rid.startsWith("wk-") || rid.startsWith("wp-") || rid.startsWith("wd-") ->
                        flash("wake: $msg")
                    rid.startsWith("wl-") -> renderWakes(emptyList(), "no wakes (mule down?)")
                    rid.startsWith("pp-") -> flash("profile save failed: $msg")
                    rid.startsWith("al-") -> { /* registry refresh is silent */ }
                }
            }
        }
    }

    private fun paintChip() = AgentForm.paintChip(profileChip, pickedProfile, profiles)

    // ---- wake rows ----

    private fun renderWakes(wakes: List<Term.WakeInfo>, empty: String? = null) {
        wakeBox.removeAllViews()
        if (wakes.isEmpty()) {
            wakeBox.addView(TextView(this).apply {
                text = empty ?: "no wakes — this agent only wakes when you talk to it"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF6b7280.toInt())
                setPadding(0, dp(4), 0, dp(4))
            })
            return
        }
        for (w in wakes) {
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(0, dp(6), 0, dp(6))
            }
            row.addView(TextView(this).apply {
                val sched = w.cron ?: w.kind
                text = (if (w.enabled) "⏰ " else "⏸ ") + sched +
                    (if (w.name.isNotEmpty()) "  · ${w.name}" else "")
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(if (w.enabled) 0xFFE5E5E5.toInt() else 0xFF6b7280.toInt())
            })
            if (w.prompt.isNotEmpty()) row.addView(TextView(this).apply {
                text = "\"${w.prompt.take(90)}\""
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(0xFF9aa0a6.toInt())
            })
            if (w.nextDueAt != null) row.addView(TextView(this).apply {
                text = "next ${w.nextDueAt.take(16)}"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
                setTextColor(0xFF6b7280.toInt())
            })
            val btns = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
            btns.addView(Button(this).apply {
                text = "edit"; isAllCaps = false; setTextColor(0xFF7FD4FF.toInt())
                setPadding(dp(8), 0, dp(8), 0)
                setOnClickListener { showWakeForm(w) }
            })
            btns.addView(Button(this).apply {
                text = if (w.enabled) "pause" else "resume"
                isAllCaps = false; setTextColor(0xFFF59E0B.toInt())
                setPadding(dp(8), 0, dp(8), 0)
                setOnClickListener {
                    relay?.send(
                        Term.wakePut(
                            agentId = agent.id,
                            cron = w.cron ?: "",
                            prompt = w.prompt.ifEmpty { " " },
                            wakeId = w.id,
                            enabled = !w.enabled,
                        )
                    )
                    flash(if (w.enabled) "pausing wake…" else "resuming wake…")
                }
            })
            btns.addView(Button(this).apply {
                text = "✕"; isAllCaps = false; setTextColor(0xFF6B7280.toInt())
                setPadding(dp(8), 0, dp(8), 0)
                setOnClickListener {
                    android.app.AlertDialog.Builder(this@EditAgentActivity)
                        .setTitle("Delete wake")
                        .setMessage("Delete \"${w.name.ifEmpty { w.id.take(8) }}\"? The agent stops hearing it.")
                        .setPositiveButton("delete") { _, _ ->
                            relay?.send(Term.wakeDelete(w.id))
                            flash("deleting wake…")
                        }
                        .setNegativeButton("cancel", null)
                        .show()
                }
            })
            row.addView(btns)
            wakeBox.addView(row)
        }
    }

    /** One wake, created ([w] == null) or edited. Schedule accepts
     *  HH:MM (daily) or a 5-field cron expr — the same input the ＋Agent
     *  builder takes; the daemon validates it before mule sees it. */
    private fun showWakeForm(w: Term.WakeInfo?) {
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        val sched = AgentForm.field(
            this, body, "wake time: HH:MM or 5-field cron", "07:00",
            initial = w?.cron ?: "",
        )
        val prompt = AgentForm.field(
            this, body, "wake prompt", "Daily check-in for ${agent.name}.",
            multiline = true, initial = w?.prompt ?: "",
        )
        val nameEdit = AgentForm.field(
            this, body, "wake name (blank = wake-${agent.name.ifEmpty { "agent" }})", "",
            initial = w?.name ?: "",
        )
        android.app.AlertDialog.Builder(this)
            .setTitle(if (w == null) "New wake" else "Edit wake")
            .setView(body)
            .setPositiveButton("save") { _, _ ->
                val s = sched.text.toString().trim()
                val p = prompt.text.toString().trim()
                if (s.isEmpty()) { flash("a wake needs a schedule"); return@setPositiveButton }
                if (p.isEmpty()) { flash("a wake needs a prompt"); return@setPositiveButton }
                relay?.send(
                    Term.wakePut(
                        agentId = agent.id,
                        cron = s,
                        prompt = p,
                        wakeId = w?.id,
                        name = nameEdit.text.toString().trim(),
                    )
                )
                flash(if (w == null) "creating wake…" else "saving wake…")
            }
            .setNegativeButton("cancel", null)
            .create()
            .show()
    }

    // ---- chrome helpers ----

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
            addView(TextView(this@EditAgentActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@EditAgentActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
