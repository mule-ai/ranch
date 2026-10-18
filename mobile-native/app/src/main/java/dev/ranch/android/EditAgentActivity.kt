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
    private lateinit var sandboxBox: LinearLayout
    private lateinit var secretsBox: LinearLayout
    private lateinit var saveBtn: Button
    private lateinit var profileChip: Button

    // Secrets section state (2026-10-18): the owner's credential
    // slots + the Vaultwarden item names (picker) + THIS agent's
    // injected set (checkboxes → AgentUpdate.env_refs on Save).
    private var vaultItems: List<String> = emptyList()
    private var vaultEnabled = false
    private var lastSecrets: JSONObject? = null
    private var pendingRefs: List<String>? = null

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
                // ---- credential slots + vault picker (2026-10-18) ----
                addView(AgentForm.label(this@EditAgentActivity, "secrets (vault-backed injection)", 14))
                secretsBox = LinearLayout(this@EditAgentActivity).apply {
                    orientation = LinearLayout.VERTICAL
                }
                addView(secretsBox)
                val scBtns = LinearLayout(this@EditAgentActivity).apply {
                    orientation = LinearLayout.HORIZONTAL
                }
                scBtns.addView(Button(this@EditAgentActivity).apply {
                    text = "＋ new slot"
                    isAllCaps = false; setTextColor(0xFF7FD4FF.toInt())
                    setPadding(dp(8), 0, dp(8), 0)
                    setOnClickListener { showSlotForm(null) }
                })
                addView(scBtns)

                // ---- sandbox packages (forge governance, 2026-10-18) ----
                addView(AgentForm.label(this@EditAgentActivity, "sandbox packages (forge)", 14))
                sandboxBox = LinearLayout(this@EditAgentActivity).apply {
                    orientation = LinearLayout.VERTICAL
                }
                addView(sandboxBox)
                val sbBtns = LinearLayout(this@EditAgentActivity).apply {
                    orientation = LinearLayout.HORIZONTAL
                }
                sbBtns.addView(Button(this@EditAgentActivity).apply {
                    text = "＋ request package"
                    isAllCaps = false; setTextColor(0xFF7FD4FF.toInt())
                    setPadding(dp(8), 0, dp(8), 0)
                    setOnClickListener { showPackageRequestForm() }
                })
                sbBtns.addView(Button(this@EditAgentActivity).apply {
                    text = "reinstall sandbox"
                    isAllCaps = false; setTextColor(0xFF6B7280.toInt())
                    setPadding(dp(8), 0, dp(8), 0)
                    setOnClickListener {
                        android.app.AlertDialog.Builder(this@EditAgentActivity)
                            .setTitle("Reinstall sandbox")
                            .setMessage("Wipe the agent's Debian trees? Its files are kept; " +
                                "approved packages reinstall automatically on the next bash run.")
                            .setPositiveButton("reinstall") { _, _ ->
                                relay?.send(Term.sandboxReset(agent.id))
                                flash("reinstalling sandbox…")
                            }
                            .setNegativeButton("cancel", null)
                            .show()
                    }
                })
                addView(sbBtns)
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
        pendingRefs = agent.envRefs   // checkbox seed until SecretsListOk paints
        status.text = "editing \"${agent.name.ifEmpty { agent.id.take(8) }}\"" +
            (if (agent.paused == true) " · PAUSED" else "")

        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.profileList())
        r.send(Term.modelCatalog())
        r.send(Term.agentList())        // refresh our row (paused state, columns)
        r.send(Term.wakeList(agent.id))
        r.send(Term.sandboxPackages(agent.id))
        r.send(Term.secretsList())
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

        // injected credential slots: the checkboxes are authoritative
        // once they've painted (pendingRefs); only sent when the set
        // actually differs from the agent's current env_refs
        val currentRefs = agent.envRefs ?: emptyList()
        val refsPatch = pendingRefs?.takeIf { it.sorted() != currentRefs.sorted() }

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
            envRefs = refsPatch,
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
                        // re-anchor the injected checkboxes to the
                        // fresh row (unless untouched — pendingRefs
                        // starts equal) and repaint the section
                        if (pendingRefs == null || pendingRefs == agent.envRefs) {
                            pendingRefs = a.envRefs
                        }
                        lastSecrets?.let { renderSecrets(it) }
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
            "SandboxPackagesOk" -> renderSandbox(f.optJSONObject("data") ?: JSONObject())
            "SandboxDecideOk" -> {
                val pkg = f.optJSONObject("data")?.optString("package", "?") ?: "?"
                flash(if (f.optJSONObject("data")?.optString("status") == "approved")
                    "approved $pkg — installing" else "rejected $pkg", green = true)
                relay?.send(Term.sandboxPackages(agent.id))
            }
            "SandboxRequestOk" -> {
                val n = f.optJSONObject("data")?.optJSONArray("requested")?.length() ?: 0
                flash(if (n > 0) "requested $n package(s) — the approver has been notified"
                     else "nothing requested (already pending?)", green = n > 0)
                relay?.send(Term.sandboxPackages(agent.id))
            }
            "SandboxResetOk" -> {
                flash("sandbox reinstalled", green = true)
                relay?.send(Term.sandboxPackages(agent.id))
            }
            "SecretsListOk" -> {
                lastSecrets = f
                vaultItems = f.optJSONArray("vault_items")?.let { arr ->
                    (0 until arr.length()).map { arr.optString(it) }
                } ?: emptyList()
                vaultEnabled = f.optBoolean("vault_enabled", false)
                renderSecrets(f)
            }
            "SecretsBindOk" -> {
                val s = f.optJSONObject("slot")
                flash("slot ${s?.optString("name", "?")} → " +
                    (s?.optString("bw_ref")?.takeIf { it.isNotEmpty() && it != "null" }
                        ?: "literal value"), green = true)
                relay?.send(Term.secretsList())
            }
            "SecretsUnbindOk" -> {
                flash(if (f.optBoolean("removed", false)) "slot removed" else "no such slot",
                    green = f.optBoolean("removed", false))
                relay?.send(Term.secretsList())
                relay?.send(Term.agentList()) // injected rows may go stale
            }
            "Error" -> {
                val rid = f.optString("req_id")
                val msg = f.optString("message", "error")
                when {
                    rid.startsWith("au-") -> { busy = false; flash("save failed: $msg") }
                    rid.startsWith("wk-") || rid.startsWith("wp-") || rid.startsWith("wd-") ->
                        flash("wake: $msg")
                    rid.startsWith("wl-") -> renderWakes(emptyList(), "no wakes (mule down?)")
                    rid.startsWith("sp-") -> renderSandboxUnavailable("forge down?")
                    rid.startsWith("sd-") -> flash("decision failed: $msg")
                    rid.startsWith("sq-") -> flash("request failed: $msg")
                    rid.startsWith("sr-") -> flash("reinstall failed: $msg")
                    rid.startsWith("sc-") -> renderSecretsUnavailable(msg)
                    rid.startsWith("sb-") -> flash("slot bind failed: $msg")
                    rid.startsWith("su-") -> flash("slot remove failed: $msg")
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

    // ---- sandbox package rows ----

    private fun renderSandboxUnavailable(msg: String) {
        sandboxBox.removeAllViews()
        sandboxBox.addView(TextView(this).apply {
            text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6b7280.toInt()); setPadding(0, dp(4), 0, dp(4))
        })
    }

    /** One bucket row set of forge's sandbox payload. Pending rows get
     *  the approve/reject doors (user decisions land as decided_by:
     *  "user" — same authority as the approver agent, so the human
     *  can always overrule from the phone). */
    private fun renderSandbox(data: JSONObject) {
        sandboxBox.removeAllViews()
        val approver = data.optJSONObject("approver")?.optString("name", "") ?: ""
        sandboxBox.addView(TextView(this).apply {
            text = "base ready: ${data.optBoolean("base_ready", false)}" +
                if (approver.isNotEmpty()) "  · approver: $approver" else "  · approver: none — you decide"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6b7280.toInt())
            setPadding(0, dp(4), 0, dp(2))
        })
        var any = false
        for ((bucket, label) in listOf(
            "pending" to "PENDING",
            "approved" to "installing",
            "installed" to "installed",
            "rejected" to "rejected",
        )) {
            val arr = data.optJSONArray(bucket) ?: continue
            if (arr.length() == 0) continue
            any = true
            sandboxBox.addView(TextView(this).apply {
                text = label
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setTextColor(0xFF9aa0a6.toInt())
                setPadding(0, dp(6), 0, 0)
            })
            for (i in 0 until arr.length()) {
                val r = arr.optJSONObject(i) ?: continue
                val id = r.optString("id")
                val pkg = r.optString("package")
                val why = r.optString("reason", "")
                val dec = r.optString("decided_by", "")
                val row = LinearLayout(this).apply {
                    orientation = LinearLayout.VERTICAL
                    setPadding(0, dp(4), 0, dp(4))
                }
                row.addView(TextView(this).apply {
                    text = "📦 $pkg" + if (why.isNotEmpty()) "  · ${why.take(80)}" else ""
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                    setTextColor(if (bucket == "rejected") 0xFF6b7280.toInt() else 0xFFE5E5E5.toInt())
                })
                val meta = buildString {
                    if (dec.isNotEmpty()) append("by $dec  ")
                    if (bucket == "pending") append("asked ${r.optString("created_at", "").take(16)}")
                    if (bucket == "installed" || bucket == "approved")
                        r.optString("install_error", "").takeIf { it.isNotEmpty() }?.let {
                            append("⚠ install failed: ${it.take(70)}")
                        }
                }
                if (meta.isNotEmpty()) row.addView(TextView(this).apply {
                    text = meta.trim()
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
                    setTextColor(0xFF6b7280.toInt())
                })
                if (bucket == "pending" && id.isNotEmpty()) {
                    val btns = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
                    btns.addView(Button(this).apply {
                        text = "✓ approve"; isAllCaps = false
                        setTextColor(0xFF34d399.toInt()); setPadding(dp(8), 0, dp(8), 0)
                        setOnClickListener {
                            relay?.send(Term.sandboxDecide(id, true))
                            flash("approving $pkg…")
                        }
                    })
                    btns.addView(Button(this).apply {
                        text = "✕ reject"; isAllCaps = false
                        setTextColor(0xFFF87171.toInt()); setPadding(dp(8), 0, dp(8), 0)
                        setOnClickListener {
                            val input = EditText(this@EditAgentActivity).apply {
                                hint = "reason (the agent is told this)"
                                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                            }
                            android.app.AlertDialog.Builder(this@EditAgentActivity)
                                .setTitle("Reject $pkg")
                                .setView(input)
                                .setPositiveButton("reject") { _, _ ->
                                    relay?.send(
                                        Term.sandboxDecide(
                                            id, false,
                                            input.text.toString().trim().ifEmpty { null },
                                        )
                                    )
                                    flash("rejecting $pkg…")
                                }
                                .setNegativeButton("cancel", null)
                                .show()
                        }
                    })
                    row.addView(btns)
                }
                sandboxBox.addView(row)
            }
        }
        if (!any) sandboxBox.addView(TextView(this).apply {
            text = "base image only — agents ask via request_sandbox_packages, or tap ＋"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6b7280.toInt()); setPadding(0, dp(4), 0, dp(4))
        })
    }

    /** User-side package request (comma/space separated names). Lands
     *  as a normal pending request — the approver chain stays intact
     *  even when you provision it yourself. */
    private fun showPackageRequestForm() {
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        val pkgs = AgentForm.field(this, body, "packages (space or comma separated)", "jq, htop")
        val reason = AgentForm.field(this, body, "reason (shown to the approver)", "needed for…", multiline = true)
        android.app.AlertDialog.Builder(this)
            .setTitle("Request sandbox packages")
            .setView(body)
            .setPositiveButton("request") { _, _ ->
                val list = pkgs.text.toString()
                    .split(Regex("[\\s,]+"))
                    .map { it.trim().lowercase() }
                    .filter { it.isNotEmpty() }
                if (list.isEmpty()) { flash("name at least one package"); return@setPositiveButton }
                relay?.send(
                    Term.sandboxRequest(
                        agent.id, list,
                        reason.text.toString().trim().ifEmpty { null },
                    )
                )
                flash("requesting ${list.joinToString(", ")}…")
            }
            .setNegativeButton("cancel", null)
            .create()
            .show()
    }

    // ---- secrets (credential slots + vault picker, 2026-10-18) ----

    private fun renderSecretsUnavailable(msg: String) {
        secretsBox.removeAllViews()
        secretsBox.addView(TextView(this).apply {
            text = "slots unavailable \u2014 $msg"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6b7280.toInt()); setPadding(0, dp(4), 0, dp(4))
        })
    }

    /** One row per OWNER slot (slots are user-level; agents merely
     *  name them). The checkbox is THIS agent's injection (the
     *  credentials_scope.env_refs mapping, written by Save); \u270e rebinds
     *  (point the slot at a different vault item), \u2715 deletes. */
    private fun renderSecrets(f: JSONObject) {
        secretsBox.removeAllViews()
        secretsBox.addView(TextView(this).apply {
            text = "vault: " + if (vaultEnabled) "configured \u00b7 ${vaultItems.size} item(s)"
                   else "not configured \u2014 literal slots only (set it up: PUT /bitwarden)"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6b7280.toInt())
            setPadding(0, dp(4), 0, dp(2))
        })
        if (pendingRefs == null) pendingRefs = agent.envRefs
        val refs = pendingRefs ?: emptyList()
        val arr = f.optJSONArray("slots")
        if (arr == null || arr.length() == 0) {
            secretsBox.addView(TextView(this).apply {
                text = "no slots yet \u2014 \uff0b one, then check it to inject it into this agent"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF6b7280.toInt()); setPadding(0, dp(4), 0, dp(4))
            })
            return
        }
        for (i in 0 until arr.length()) {
            val sl = arr.optJSONObject(i) ?: continue
            val name = sl.optString("name")
            if (name.isEmpty()) continue
            val isVault = sl.optBoolean("vault", false)
            val bwRef = sl.optString("bw_ref", "")
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(0, dp(4), 0, dp(4))
            }
            val top = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
            val cb = android.widget.CheckBox(this).apply {
                isChecked = name in refs
                contentDescription = "inject $name"
                setOnCheckedChangeListener { _, on ->
                    val cur = pendingRefs ?: agent.envRefs ?: emptyList()
                    pendingRefs = if (on) (cur + name).distinct() else cur - name
                }
            }
            top.addView(cb)
            top.addView(TextView(this).apply {
                text = (if (isVault) "\ud83d\udd10 " else "\ud83d\udd11 ") + name
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFE5E5E5.toInt())
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            })
            top.addView(Button(this).apply {
                text = "\u270e"; isAllCaps = false; setTextColor(0xFF7FD4FF.toInt())
                setPadding(dp(8), 0, dp(8), 0)
                setOnClickListener { showSlotForm(name) }
            })
            top.addView(Button(this).apply {
                text = "\u2715"; isAllCaps = false; setTextColor(0xFFF87171.toInt())
                setPadding(dp(8), 0, dp(8), 0)
                setOnClickListener {
                    android.app.AlertDialog.Builder(this@EditAgentActivity)
                        .setTitle("Delete slot $name?")
                        .setMessage(if (isVault)
                            "Removes the vault binding (the vault item itself is untouched). " +
                                "Agents still naming it will report it unavailable."
                        else "Agents still naming it will report it unavailable.")
                        .setPositiveButton("delete") { _, _ ->
                            relay?.send(Term.secretsUnbind(name))
                            flash("removing $name\u2026")
                        }
                        .setNegativeButton("cancel", null)
                        .show()
                }
            })
            row.addView(top)
            val meta = when {
                isVault && bwRef.isNotEmpty() && bwRef != "null" -> bwRef
                isVault -> "vault-backed"
                else -> "literal \u00b7 value never leaves forge"
            }
            row.addView(TextView(this).apply {
                text = meta
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
                setTextColor(0xFF6b7280.toInt())
                setPadding(dp(36), 0, 0, 0)
            })
            secretsBox.addView(row)
        }
    }

    /** Create a slot (name null) or rebind one (name given). Source:
     *  a Vaultwarden item from the picker (stores bw://notes/<item>),
     *  or a literal value typed here. The secret VALUE is fetched by
     *  forge at exec time \u2014 only the pointer rides this dialog for
     *  vault slots. */
    private fun showSlotForm(existing: String?) {
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        val nameField = EditText(this).apply {
            hint = "SLOT_NAME (A-Z, 0-9, _)"
            setText(existing ?: "")
            isEnabled = existing == null
            inputType = android.text.InputType.TYPE_CLASS_TEXT or
                android.text.InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS
        }
        body.addView(nameField)
        val srcOptions = mutableListOf<String>()
        if (vaultEnabled) srcOptions.add("vault item")
        srcOptions.add("literal value")
        val srcSpinner = android.widget.Spinner(this)
        srcSpinner.adapter = android.widget.ArrayAdapter(
            this, android.R.layout.simple_spinner_dropdown_item, srcOptions
        )
        body.addView(srcSpinner)
        val itemOptions = mutableListOf<String>()
        val cur = lastSecrets?.optJSONArray("slots")?.let { a ->
            (0 until a.length()).asSequence()
                .mapNotNull { a.optJSONObject(it) }
                .firstOrNull { it.optString("name") == existing }
        }
        val curRef = cur?.optString("bw_ref", "").orEmpty()
        var preselectItem: String? = null
        if (vaultEnabled) {
            if (curRef.startsWith("bw://notes/"))
                preselectItem = curRef.removePrefix("bw://notes/")
            itemOptions.addAll(vaultItems)
            if (preselectItem != null && preselectItem !in itemOptions) itemOptions.add(preselectItem)
            itemOptions.add("\u2026type manually")
        }
        val itemSpinner = android.widget.Spinner(this)
        itemSpinner.adapter = android.widget.ArrayAdapter(
            this, android.R.layout.simple_spinner_dropdown_item, itemOptions
        )
        if (preselectItem != null && preselectItem in itemOptions)
            itemSpinner.setSelection(itemOptions.indexOf(preselectItem))
        val itemManual = EditText(this).apply {
            hint = "vault item name"
            visibility = View.GONE
        }
        val literalField = EditText(this).apply {
            hint = "the value (stored server-side, never listed back)"
            visibility = View.GONE
            inputType = android.text.InputType.TYPE_CLASS_TEXT or
                android.text.InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        body.addView(itemSpinner)
        body.addView(itemManual)
        body.addView(literalField)
        fun syncSource() {
            val vault = srcOptions[srcSpinner.selectedItemPosition] == "vault item"
            itemSpinner.visibility = if (vault) View.VISIBLE else View.GONE
            itemManual.visibility =
                if (vault && itemOptions.getOrElse(itemSpinner.selectedItemPosition) { "" }
                        .startsWith("\u2026")) View.VISIBLE else View.GONE
            literalField.visibility = if (vault) View.GONE else View.VISIBLE
        }
        srcSpinner.onItemSelectedListener = object : android.widget.AdapterView.OnItemSelectedListener {
            override fun onItemSelected(p: android.widget.AdapterView<*>?, v: View?, pos: Int, id: Long) = syncSource()
            override fun onNothingSelected(p: android.widget.AdapterView<*>?) {}
        }
        if (existing != null && !vaultEnabled) srcSpinner.setSelection(srcOptions.size - 1)
        syncSource()
        android.app.AlertDialog.Builder(this)
            .setTitle(if (existing == null) "New credential slot" else "Rebind $existing")
            .setView(body)
            .setPositiveButton("save") { _, _ ->
                val nm = nameField.text.toString().trim().uppercase()
                if (nm.isEmpty()) { flash("slot needs a name"); return@setPositiveButton }
                if (!Regex("^[A-Z][A-Z0-9_]{0,63}$").matches(nm)) {
                    flash("slot name: A-Z start, A-Z0-9_ after"); return@setPositiveButton
                }
                val vault = srcOptions[srcSpinner.selectedItemPosition] == "vault item"
                if (vault) {
                    val picked = itemOptions.getOrElse(itemSpinner.selectedItemPosition) { "" }
                    val item = if (picked.startsWith("\u2026")) itemManual.text.toString().trim() else picked
                    if (item.isEmpty()) { flash("pick or type a vault item"); return@setPositiveButton }
                    relay?.send(Term.secretsBind(nm, vaultItem = item))
                    flash("binding $nm \u2192 $item\u2026")
                } else {
                    val v = literalField.text.toString()
                    if (v.isEmpty()) { flash("enter a value"); return@setPositiveButton }
                    relay?.send(Term.secretsBind(nm, literal = v))
                    flash("saving $nm\u2026")
                }
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
