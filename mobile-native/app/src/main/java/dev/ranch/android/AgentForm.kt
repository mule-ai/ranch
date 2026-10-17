package dev.ranch.android

import android.app.Activity
import android.app.AlertDialog
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/**
 * Shared form plumbing for the two herd-agent screens: the H6.5 ＋Agent
 * builder ([NewAgentActivity]) and the H6.5b ✎ editor
 * ([EditAgentActivity]). Both screens build the same three things —
 * a labelled text field, a **model-profile picker** (the daemon's
 * `ProfileList` + an inline `ProfilePut` new-profile form backed by
 * `ModelCatalog`), and the **tools allowlist** checkbox set — and the
 * editor must show exactly what the builder wrote, so they share one
 * implementation instead of two that drift.
 *
 * Pure UI: no frames are sent from here except through the `send`
 * callback the caller passes (the screens own the relay sink and their
 * own req-id prefixes: `pp-` profile put, `apv-` provision,
 * `au-` update, `wk-` wakes).
 */
object AgentForm {

    /** The harness tool names the allowlist offers (agents_new.rs KNOWN_TOOLS). */
    val KNOWN_TOOLS = listOf(
        "bash", "read", "write", "webfetch", "search",
        "web_login", "web_login_done", "spawn_subagent",
        "memory_remember", "agent_signal", "schedule_reminder",
    )

    /** One forge profile row (id, display name, "provider/model"). */
    data class ProfileRow(val id: String, val name: String, val providerModel: String)

    /** One `ModelCatalogOk` row (provider, id, name). */
    data class ModelRow(val provider: String, val id: String, val name: String)

    fun parseProfiles(arr: JSONArray?): List<ProfileRow> {
        if (arr == null) return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val p = arr.optJSONObject(i) ?: return@mapNotNull null
            val id = p.optString("id")
            if (id.isEmpty()) return@mapNotNull null
            ProfileRow(id, p.optString("name", id), "${p.optString("provider")}/${p.optString("model")}")
        }
    }

    fun parseModels(arr: JSONArray?): List<ModelRow> {
        if (arr == null) return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val m = arr.optJSONObject(i) ?: return@mapNotNull null
            ModelRow(m.optString("provider"), m.optString("id"), m.optString("name"))
        }
    }

    fun dp(a: Activity, v: Int): Int = (v * a.resources.displayMetrics.density).toInt()

    /** Small grey section label above a control. */
    fun label(a: Activity, text: String, topPad: Int = 8): TextView =
        TextView(a).apply {
            this.text = text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt())
            setPadding(0, dp(a, topPad), 0, dp(a, 2))
        }

    /** A labelled single-line (or multiline) text field appended to [parent]. */
    fun field(
        a: Activity,
        parent: LinearLayout,
        label: String,
        hint: String,
        multiline: Boolean = false,
        initial: String = "",
    ): EditText {
        parent.addView(label(a, label))
        val edit = EditText(a).apply {
            this.hint = hint
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
            setHintTextColor(0xFF4b5563.toInt())
            if (initial.isNotEmpty()) setText(initial)
            if (multiline) {
                minLines = 3
                setSingleLine(false)
                gravity = Gravity.TOP or Gravity.START
            } else setSingleLine(true)
            background.setTint(0xFF9aa0a6.toInt())
        }
        parent.addView(edit)
        return edit
    }

    /** Paint the profile chip's label for the current selection. */
    fun paintChip(chip: Button, picked: String, profiles: List<ProfileRow>) {
        chip.text = when {
            picked.isNotEmpty() -> {
                val p = profiles.firstOrNull { it.id == picked }
                "profile: ${p?.name ?: picked.take(8)}${if (p != null) " (${p.providerModel})" else ""}"
            }
            profiles.isEmpty() -> "no profiles — forge unreachable? (use ＋ new profile)"
            else -> "profile: none (blank agent)"
        }
    }

    /** The profile picker dialog: one row per profile + "none". */
    fun showProfilePicker(
        a: Activity,
        profiles: List<ProfileRow>,
        current: String,
        onPick: (String) -> Unit,
    ) {
        val body = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(a, 8), dp(a, 8), dp(a, 8), dp(a, 8))
        }
        val dialog = AlertDialog.Builder(a)
            .setTitle("Choose the agent's model profile")
            .setView(body)
            .create()
        fun addRow(line: String, sub: String, pick: String) {
            body.addView(Button(a).apply {
                text = if (sub.isEmpty()) line else "$line\n$sub"
                isAllCaps = false
                gravity = Gravity.START or Gravity.CENTER_VERTICAL
                setTextColor(0xFFE5E5E5.toInt())
                setPadding(dp(a, 10), dp(a, 8), dp(a, 10), dp(a, 8))
                setOnClickListener {
                    onPick(pick)
                    dialog.dismiss()
                }
                if (pick == current) setBackgroundColor(0xFF1d2733.toInt())
            })
        }
        addRow("no profile", "the agent has no model until one is attached", "")
        for (p in profiles) addRow(p.name, p.providerModel, p.id)
        dialog.show()
    }

    /** Inline new-profile form (`ProfilePut`). Model rows come from the
     *  catalog; a custom model id works too when the catalog is empty.
     *  [onSaved] is invoked with the profile id when `ProfilePutOk`
     *  arrives — the caller maps frames, this only sends the request. */
    fun showNewProfileForm(
        a: Activity,
        catalog: List<ModelRow>,
        send: (JSONObject) -> Unit,
    ) {
        val body = LinearLayout(a).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(a, 8), dp(a, 8), dp(a, 8), dp(a, 8))
        }
        val pName = field(a, body, "name", "e.g. \"spark work\"")
        val provider = field(a, body, "provider", "proxy / openai / anthropic / …")
        val modelEdit = field(a, body, "model", "provider/model (or see model below)")
        val baseUrl = field(a, body, "base url (blank = provider default)", "http://…")
        val apiKey = field(a, body, "api key (blank = use forge's stored key)", "sk_…")
        val workDir = field(a, body, "working dir (blank = none)", "/root/…")
        val prompt = field(a, body, "system prompt (blank = none)", "You are …", multiline = true)

        if (catalog.isNotEmpty()) {
            body.addView(label(a, "or pick a catalog model:"))
            val pick = ScrollView(a)
            val inner = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }
            for (m in catalog) {
                inner.addView(Button(a).apply {
                    text = m.id
                    isAllCaps = false
                    setTextColor(0xFFE5E5E5.toInt())
                    setPadding(0, dp(a, 2), 0, dp(a, 2))
                    setOnClickListener {
                        provider.setText(m.provider)
                        modelEdit.setText("${m.provider}/${m.id}")
                    }
                }, LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT,
                    LinearLayout.LayoutParams.WRAP_CONTENT
                ).apply { setMargins(0, 0, dp(a, 6), 0) })
            }
            pick.addView(inner)
            body.addView(pick, LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, dp(a, 160)
            ))
        }

        AlertDialog.Builder(a)
            .setTitle("New forge profile")
            .setView(body)
            .setPositiveButton("save profile") { _, _ ->
                val n = pName.text.toString().trim()
                val prov = provider.text.toString().trim()
                val md = modelEdit.text.toString().trim()
                if (n.isEmpty() || prov.isEmpty() || md.isEmpty()) {
                    android.widget.Toast.makeText(
                        a, "name, provider and model are required", android.widget.Toast.LENGTH_SHORT
                    ).show()
                    return@setPositiveButton
                }
                send(
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
            .show()
    }

    /** The tools-allowlist checkbox set. Returns the row view to add to
     *  the form and a getter for the checked tool names (empty = all
     *  tools, the forge/H1.1 empty-allowlist convention). */
    fun toolsBox(a: Activity, checked: Set<String>): Pair<View, () -> List<String>> {
        val box = LinearLayout(a).apply { orientation = LinearLayout.VERTICAL }
        val checks = mutableListOf<CheckBox>()
        for (t in KNOWN_TOOLS) {
            val cb = CheckBox(a).apply {
                text = t
                setTextColor(0xFFE5E5E5.toInt())
                textSize = 13f
                setPadding(0, dp(a, 2), 0, dp(a, 2))
                isChecked = t in checked
            }
            checks.add(cb)
            box.addView(cb)
        }
        return Pair(box) { checks.filter { it.isChecked }.map { it.text.toString() } }
    }
}
