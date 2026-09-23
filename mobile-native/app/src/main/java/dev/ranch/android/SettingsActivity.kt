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
                appendLine("frames received: ${d["frames"]}")
                appendLine("fired: turn=${d["firedTurn"]} msg=${d["firedMsg"]} q=${d["firedQ"]}")
                appendLine("skipped: active=${d["skipActive"]} off=${d["skipOff"]}  errors=${d["errors"]}")
            }
            handler.postDelayed(this, 2000)
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
        toolBtn(tools1, "Files", EditorActivity::class.java)
        toolBtn(tools2, "Workflows", WorkflowsActivity::class.java)
        toolBtn(tools2, "Triggers", TriggersActivity::class.java)
        toolBtn(tools2, "Machines", MachinesActivity::class.java)
        root.addView(tools1)
        root.addView(tools2)

        // ---- notifications ----
        root.addView(sectionHeader("Notifications"))
        root.addView(buildSwitch("turn_end", "Agent finished turn", app.prefs.getBool("turn_end", true)))
        root.addView(buildSwitch("every_message", "Every agent message", app.prefs.getBool("every_message", false)))
        root.addView(buildSwitch("ignore_tool_calls", "Ignore tool calls", app.prefs.getBool("ignore_tool_calls", true)))
        root.addView(buildSwitch("questions", "Agent questions", app.prefs.getBool("questions", true)))
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

    private fun requestNotifPermission() {
        if (Build.VERSION.SDK_INT >= 33) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1)
        } else {
            diagText.text = "permission not required (pre-Android 13)"
        }
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
        }
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}
