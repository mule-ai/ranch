package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import java.util.concurrent.Executors

/**
 * Settings & tools (opened from the monitor home screen): the 4
 * notification toggles, permission + test-notification buttons, the
 * Phase-4 tool screens (Agents / Files / Workflows / Triggers / Machines),
 * and the live diagnostics readout.
 */
class SettingsActivity : Activity() {

    private val app get() = application as App
    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()
    private lateinit var diagText: TextView

    private val diagRunnable = object : Runnable {
        override fun run() {
            val d = Monitor.diag()
            diagText.text = buildString {
                appendLine("status: ${d["status"]}  machine: ${d["machine"]}")
                appendLine("version: ${versionLabel()}")
                appendLine("frames received: ${d["frames"]}")
                appendLine("fired: turn=${d["firedTurn"]} msg=${d["firedMsg"]} q=${d["firedQ"]}")
                appendLine("skipped: active=${d["skipActive"]} off=${d["skipOff"]}  errors=${d["errors"]}")
            }
            handler.postDelayed(this, 2000)
        }
    }

    /// The installed build identity ("<versionName> (code N) · <baked tag>")
    /// — so a given APK is distinguishable from an older one on the device
    /// without adb.
    private fun versionLabel(): String {
        return try {
            val p = packageManager.getPackageInfo(packageName, 0)
            "${p.versionName ?: "?"} (code ${p.versionCode}) · ${Version.own}"
        } catch (e: Exception) {
            Version.own
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(16), dp(16), dp(16), dp(16))
        }
        setContentView(ScrollView(this).apply { addView(root) })
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))

        val bar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        bar.addView(Button(this).apply { text = "←"; setOnClickListener { finish() } })
        bar.addView(TextView(this).apply {
            text = "Settings & tools"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        // ---- tools ----
        root.addView(sectionHeader("Tools"))
        val tools1 = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val tools2 = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        fun toolBtn(row: LinearLayout, label: String, cls: Class<*>) = row.addView(Button(this).apply {
            text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(dp(6), dp(8), dp(6), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener { startActivity(Intent(this@SettingsActivity, cls)) }
        })
        toolBtn(tools1, "Agents", AgentsActivity::class.java)
        toolBtn(tools1, "Activity", HerdActivity::class.java)
        toolBtn(tools1, "Files", EditorActivity::class.java)
        toolBtn(tools2, "Per-agent", NotifyAgentsActivity::class.java)
        toolBtn(tools2, "Workflows", WorkflowsActivity::class.java)
        toolBtn(tools2, "Triggers", TriggersActivity::class.java)
        toolBtn(tools2, "Machines", MachinesActivity::class.java)
        root.addView(tools1)
        root.addView(tools2)

        // ---- editor (IDE) ----
        root.addView(sectionHeader("Editor"))
        root.addView(buildFontRow("Font size", "editor_font_px", 14, 10, 28, 1))
        root.addView(buildFontRow("Markdown font size", "markdown_font_px", 16, 12, 30, 1))

        // ---- voice input (H6.2) ----
        root.addView(sectionHeader("Voice input"))
        root.addView(buildVoiceModeRow())
        root.addView(buildConfigField("Forge STT base URL", Voice.PKEY_FORGE_BASE,
            "e.g. http://your-machine:PORT (the forge API server)"))
        root.addView(buildConfigField("Forge API key", Voice.PKEY_FORGE_KEY, "forge API key", true))
        // H6.3 voice call (📞 on an agent row): the phone dials the
        // mule voice_call workflow directly — mule base URL + API key
        // (the same pair as daemon.toml's mule_url/mule_api_key).
        root.addView(buildConfigField("Mule base URL", CallActivity.PKEY_MULE_BASE,
            "e.g. http://your-machine:8080 (the mule API server)"))
        root.addView(buildConfigField("Mule API key", CallActivity.PKEY_MULE_KEY, "mule API key", true))
        root.addView(TextView(this).apply {
            text = "forge STT (default): hold the mic in a chat, release to transcribe via " +
                "forge's /v1/audio/transcriptions proxy (Parakeet). Needs the base URL + key " +
                "above — the phone talks to forge directly.  " +
                "recognizer: Android's on-device speech service — works in airplane mode, " +
                "no config. The transcript lands in the chat box; you still hit send."
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6B7280.toInt())
            setPadding(0, 0, 0, dp(8))
        })
        root.addView(Button(this).apply {
            text = "Microphone permission"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                requestPermissions(
                    arrayOf(android.Manifest.permission.RECORD_AUDIO), 2)
            }
        })

        // ---- notifications ----
        root.addView(sectionHeader("Notifications"))
        root.addView(buildSwitch("turn_end", "Agent finished turn (includes last message)", app.prefs.getBool("turn_end", true)))
        root.addView(buildSwitch("every_message", "Every agent message (only when turn-end is off)", app.prefs.getBool("every_message", false)))
        root.addView(buildSwitch("ignore_tool_calls", "Ignore tool calls", app.prefs.getBool("ignore_tool_calls", true)))
        root.addView(buildSwitch("questions", "Agent questions", app.prefs.getBool("questions", true)))
        root.addView(buildSwitch("approval", "Approvals (allow/deny cards)", app.prefs.getBool("approval", true)))
        root.addView(buildSwitch("research", "Research reports (use/discard)", app.prefs.getBool("research", true)))
        root.addView(buildSwitch("memory", "Memory reviews (keep/forget)", app.prefs.getBool("memory", true)))
        root.addView(buildSwitch("task_done", "Background tasks finished", app.prefs.getBool("task_done", true)))
        root.addView(Button(this).apply {
            text = "Notification permission"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener { requestNotifPermission() }
        })
        root.addView(Button(this).apply {
            text = "Send test notification"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                exec.execute {
                    val n = Notify(app)
                    val (ok, detail) = n.testNotification()
                    handler.post { diagText.text = "test: $detail" }
                }
            }
        })

        // ---- diagnostics ----
        root.addView(sectionHeader("Diagnostics"))
        diagText = TextView(this).apply {
            text = "…"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9AA0A6.toInt())
        }
        root.addView(diagText)
        handler.postDelayed(diagRunnable, 500)
    }

    override fun onDestroy() {
        handler.removeCallbacks(diagRunnable)
        exec.shutdownNow()
        super.onDestroy()
    }

    private fun sectionHeader(title: String): TextView =
        TextView(this).apply {
            text = title
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, dp(16), 0, dp(4))
        }

    private fun buildSwitch(key: String, label: String, default: Boolean): Switch =
        Switch(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            isChecked = app.prefs.getBool(key, default)
            setPadding(0, dp(2), 0, dp(2))
            setOnCheckedChangeListener { _, checked -> app.prefs.setBool(key, checked) }
        }

    /** A `label  [-] value [+]` stepper persisted as an int pref (font sizes). */
    private fun buildFontRow(label: String, key: String, def: Int, min: Int, max: Int, step: Int): LinearLayout =
        LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, dp(4), 0, dp(4))
            addView(TextView(this@SettingsActivity).apply {
                text = label
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFC9CDD3.toInt())
            }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))

            val value = intArrayOf(app.prefs.getInt(key, def))
            val valueTv = TextView(this@SettingsActivity).apply {
                text = "${value[0]}"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFE5E5E5.toInt())
                setPadding(dp(10), 0, dp(10), 0)
            }

            val minus = Button(this@SettingsActivity).apply {
                text = "\u2212"; textSize = 18f; setTextColor(0xFF7FD4FF.toInt())
                setBackgroundColor(0xFF1B2126.toInt()); setPadding(dp(14), 0, dp(14), 0)
                minWidth = 0; minimumWidth = 0
                setOnClickListener {
                    if (value[0] > min) {
                        value[0] -= step; app.prefs.setInt(key, value[0])
                        valueTv.text = value[0].toString()
                    }
                }
            }
            val plus = Button(this@SettingsActivity).apply {
                text = "+"; textSize = 18f; setTextColor(0xFF7FD4FF.toInt())
                setBackgroundColor(0xFF1B2126.toInt()); setPadding(dp(14), 0, dp(14), 0)
                minWidth = 0; minimumWidth = 0
                setOnClickListener {
                    if (value[0] < max) {
                        value[0] += step; app.prefs.setInt(key, value[0])
                        valueTv.text = value[0].toString()
                    }
                }
            }
            addView(minus); addView(valueTv); addView(plus)
        }

    private fun requestNotifPermission() {
        if (Build.VERSION.SDK_INT >= 33) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1)
        } else {
            diagText.text = "permission not required (pre-Android 13)"
        }
    }

    /** `recognizer | forge-stt` toggle (two highlight buttons; forge-stt is
     * the default — it's the path the H6.3 voice loop uses too). */
    private fun buildVoiceModeRow(): LinearLayout {
        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val modes = arrayOf(
            Voice.MODE_RECOGNIZER to "recognizer (on-device)",
            Voice.MODE_FORGE to "forge STT (default)",
        )
        fun style(b: Button, on: Boolean) {
            b.setTextColor(if (on) 0xFF4ADE80.toInt() else 0xFF9AA0A6.toInt())
            b.setBackgroundColor(if (on) 0xFF14261E.toInt() else 0xFF1B2126.toInt())
        }
        for ((key, label) in modes) {
            val b = Button(this).apply {
                text = label
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setPadding(dp(8), dp(8), dp(8), dp(8))
                minWidth = 0; minimumWidth = 0
                background = android.graphics.drawable.GradientDrawable().apply {
                    cornerRadius = dp(8).toFloat()
                }
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f).apply {
                    if (row.childCount > 0) leftMargin = dp(6)
                }
            }
            row.addView(b)
            style(b, app.prefs.get(Voice.PKEY_MODE, Voice.MODE_FORGE) == key)
            b.setOnClickListener {
                app.prefs.set(Voice.PKEY_MODE, key)
                for (c in 0 until row.childCount) style(row.getChildAt(c) as Button, c == row.indexOfChild(b))
            }
        }
        return row
    }

    /** Label + persisted EditText (saved on blur / keyboard done). The forge
     *  base URL + key give the phone direct STT access (the recognizer path
     *  needs neither). */
    private fun buildConfigField(label: String, key: String, hint: String, password: Boolean = false): LinearLayout {
        val wrap = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        wrap.addView(TextView(this@SettingsActivity).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, dp(6), 0, dp(2))
        })
        val et = EditText(this@SettingsActivity).apply {
            setText(app.prefs.get(key, ""))
            setHint(hint)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFC9CDD3.toInt())
            setHintTextColor(0xFF5C636B.toInt())
            if (password) inputType = android.text.InputType.TYPE_CLASS_TEXT or
                android.text.InputType.TYPE_TEXT_VARIATION_PASSWORD
            setPadding(0, dp(6), 0, dp(6))
            background = android.graphics.drawable.GradientDrawable().apply {
                cornerRadius = dp(8).toFloat()
                setColor(0xFF1B2126.toInt())
            }
        }
        val save = { et: EditText -> app.prefs.set(key, et.text.toString().trim()) }
        et.setOnFocusChangeListener { _, hasFocus -> if (!hasFocus) save(et) }
        et.setOnEditorActionListener { v, _, _ -> save(v as EditText); true }
        wrap.addView(et)
        return wrap
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 1) {
            diagText.text = if (grantResults.isNotEmpty() &&
                grantResults[0] == PackageManager.PERMISSION_GRANTED
            ) "notification permission granted ✓" else "notification permission denied"
        } else if (requestCode == 2) {
            diagText.text = if (grantResults.isNotEmpty() &&
                grantResults[0] == PackageManager.PERMISSION_GRANTED
            ) "microphone permission granted ✓" else "microphone permission denied"
        }
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}
