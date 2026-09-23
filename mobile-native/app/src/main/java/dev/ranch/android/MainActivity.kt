package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.ArrayAdapter
import android.widget.Switch
import android.widget.TextView
import java.util.concurrent.Executors

/**
 * Single-activity UI: login → machine picker → start/stop monitor.
 * Also hosts the 4 notification toggles + diagnostics readout.
 */
class MainActivity : Activity() {

    private val app get() = application as App

    // lazy: field initializers run at construction, before Activity.attach(),
    // where getApplication() is still null (NPE = crash on launch)
    private val auth by lazy { Auth(app.prefs) }
    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()

    // UI refs
    private lateinit var loginStatus: TextView
    private lateinit var machineStatus: TextView
    private lateinit var monitorBtn: Button
    private lateinit var monitorStatus: TextView
    private lateinit var diagText: TextView
    private lateinit var machinePicker: Spinner
    private var machines: List<Machine> = emptyList()
    private lateinit var sessionsBox: LinearLayout
    private lateinit var sessionsHeader: TextView

    private val diagRunnable = object : Runnable {
        override fun run() {
            if (Monitor.running) {
                val d = Monitor.diag()
                diagText.text = buildString {
                    appendLine("status: ${d["status"]}  machine: ${d["machine"]}")
                    appendLine("frames received: ${d["frames"]}")
                    appendLine("fired: turn=${d["firedTurn"]} errors=${d["firedError"]} msg=${d["firedMsg"]} q=${d["firedQ"]}")
                    appendLine("skipped: active=${d["skipActive"]} off=${d["skipOff"]}  errors=${d["errors"]}")
                }
                refreshSessions()
            } else {
                diagText.text = "monitor not running"
                refreshSessions()
            }
            handler.postDelayed(this, 2000)
        }
    }

    private fun refreshSessions() {
        if (!::sessionsBox.isInitialized) return
        val sessions = Monitor.sessions
        sessionsBox.removeAllViews()
        sessionsHeader.text = if (sessions.isEmpty()) "no sessions yet" else "${sessions.size} session(s)"
        if (sessions.isEmpty()) return
        for (s in sessions) {
            val b = Button(this).apply {
                val badge = when (s.kind) {
                    "forge" -> "🤖 agent"
                    "pi" -> "π pi"
                    else -> "💻 shell"
                }
                text = "${s.name.ifEmpty { s.id.take(8) }}\n$badge"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
                setPadding(dp(14), dp(12), dp(14), dp(12))
                setOnClickListener {
                    startActivity(Intent(this@MainActivity, SessionActivity::class.java)
                        .putExtra("sessionId", s.id)
                        .putExtra("sessionName", s.name))
                }
            }
            sessionsBox.addView(b)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // ---- build the UI programmatically (no XML layout needed) ----
        val pad = dp(16)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
        }
        val scroll = ScrollView(this).apply { addView(root) }
        setContentView(scroll)

        // Title
        root.addView(TextView(this).apply {
            text = "🤠 Ranch Native"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 24f)
            setPadding(0, 0, 0, dp(16))
        })

        // last-crash viewer: uncaught exceptions land in filesDir/crash.txt
        // (survives app updates) so on-device crashes are debuggable
        val crashFile = java.io.File(filesDir, "crash.txt")
        if (crashFile.exists()) {
            root.addView(Button(this).apply {
                text = "⚠ view last crash log"
                setTextColor(0xFFEF4444.toInt())
                setOnClickListener {
                    android.app.AlertDialog.Builder(this@MainActivity)
                        .setTitle("last crash")
                        .setMessage(crashFile.readText().take(4000))
                        .setPositiveButton("clear") { _, _ -> crashFile.delete() }
                        .setNegativeButton("close", null)
                        .show()
                }
            })
        }

        // --- Login section ---
        root.addView(sectionHeader("Sign in"))
        val email = EditText(this).apply { hint = "email" }
        val pw = EditText(this).apply {
            hint = "password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        root.addView(email)
        root.addView(pw)
        root.addView(Button(this).apply {
            text = "Sign in"
            setPadding(0, dp(8), 0, dp(8))
            setOnClickListener {
                loginStatus.text = "signing in…"
                exec.execute {
                    val err = auth.login(email.text.toString(), pw.text.toString())
                    handler.post {
                        loginStatus.text = if (err == null) "signed in ✓" else "error: $err"
                        if (err == null) loadMachines()
                    }
                }
            }
        })
        loginStatus = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, 0, 0, dp(16))
        }
        root.addView(loginStatus)
        root.addView(Button(this).apply {
            text = "Sign in with Google"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(auth.googleAuthorizeUrl())))
                    loginStatus.text = "completing sign-in in browser…"
                } catch (e: Exception) { loginStatus.text = "no browser: ${e.message}" }
            }
        })
        if (auth.isLoggedIn()) loginStatus.text = "signed in (restored)"

        // OAuth deep-link return: ranch://auth-callback#access_token=…
        handleAuthIntent(intent)

        // --- Machine picker ---
        root.addView(sectionHeader("Machine"))
        root.addView(Button(this).apply {
            text = "Load machines"
            setPadding(0, dp(8), 0, dp(8))
            setOnClickListener { loadMachines() }
        })
        machinePicker = Spinner(this)
        root.addView(machinePicker)
        machineStatus = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, 0, 0, dp(16))
        }
        root.addView(machineStatus)

        // --- Monitor ---
        root.addView(sectionHeader("Monitor"))
        monitorBtn = Button(this).apply {
            text = "Start monitoring"
            setPadding(0, dp(12), 0, dp(12))
            setOnClickListener { toggleMonitor() }
        }
        root.addView(monitorBtn)
        monitorStatus = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, 0, 0, dp(16))
        }
        root.addView(monitorStatus)

        // --- Sessions ---
        root.addView(sectionHeader("Sessions"))
        val newSessionRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        newSessionRow.addView(Button(this).apply {
            text = "+ shell"
            setPadding(dp(8), dp(8), dp(8), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener {
                if (Monitor.relay == null) { monitorStatus.text = "start monitoring first"; return@setOnClickListener }
                Monitor.relay?.createSession("shell")
                monitorStatus.text = "creating shell session…"
            }
        })
        newSessionRow.addView(Button(this).apply {
            text = "+ agent"
            setPadding(dp(8), dp(8), dp(8), dp(8))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener {
                if (Monitor.relay == null) { monitorStatus.text = "start monitoring first"; return@setOnClickListener }
                Monitor.relay?.createSession("pi")
                monitorStatus.text = "creating agent session…"
            }
        })
        root.addView(newSessionRow)
        sessionsHeader = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, 0, 0, dp(4))
        }
        root.addView(sessionsHeader)
        sessionsBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(sessionsBox)

        // --- Tools (Phase 4 screens) ---
        root.addView(sectionHeader("Tools"))
        val tools = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        fun toolBtn(label: String, cls: Class<*>) = Button(this).apply {
            text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener { startActivity(Intent(this@MainActivity, cls)) }
        }
        tools.addView(toolBtn("Agents", AgentsActivity::class.java))
        tools.addView(toolBtn("Files", EditorActivity::class.java))
        root.addView(tools)
        val tools2 = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        tools2.addView(toolBtn("Workflows", WorkflowsActivity::class.java))
        tools2.addView(toolBtn("Triggers", TriggersActivity::class.java))
        tools2.addView(toolBtn("Machines", MachinesActivity::class.java))
        root.addView(tools2)

        // --- Notification settings ---
        root.addView(sectionHeader("Notifications"))
        root.addView(buildSwitch("turn_end", "Agent finished turn", app.prefs.getBool("turn_end", true)))
        root.addView(buildSwitch("errors", "Agent errors", app.prefs.getBool("errors", true)))
        root.addView(buildSwitch("every_message", "Every agent message", app.prefs.getBool("every_message", false)))
        root.addView(buildSwitch("ignore_tool_calls", "Ignore tool calls", app.prefs.getBool("ignore_tool_calls", true)))
        root.addView(buildSwitch("questions", "Agent questions", app.prefs.getBool("questions", true)))

        // Permission + test buttons
        val permBtn = Button(this).apply {
            text = "Notification permission"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener { requestNotifPermission() }
        }
        root.addView(permBtn)
        root.addView(Button(this).apply {
            text = "Send test notification"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                exec.execute {
                    val n = Notify(app)
                    val (ok, detail) = n.testNotification()
                    handler.post { monitorStatus.text = "test: $detail" }
                }
            }
        })

        // --- Diagnostics ---
        root.addView(sectionHeader("Diagnostics"))
        diagText = TextView(this).apply {
            text = "monitor not running"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setPadding(0, 0, 0, 0)
        }
        root.addView(diagText)

        // periodic diag refresh
        handler.postDelayed(diagRunnable, 1000)

        // restore machine picker if we have a cached selection
        val cachedMachine = app.prefs.get("machine_id", "")
        if (cachedMachine.isNotEmpty()) loadMachines()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleAuthIntent(intent)
    }

    private fun handleAuthIntent(i: Intent?) {
        val data = i?.data ?: return
        if (data.scheme != "ranch" || data.host != "auth-callback") return
        val err = auth.applyOAuthFragment(data.fragment)
        if (!::loginStatus.isInitialized) return
        loginStatus.text = if (err == null) "signed in with Google ✓" else "google sign-in failed: $err"
        if (err == null) loadMachines()
    }

    override fun onDestroy() {
        handler.removeCallbacks(diagRunnable)
        exec.shutdownNow()
        super.onDestroy()
    }

    // ---- helpers ----

    private fun sectionHeader(title: String): TextView =
        TextView(this).apply {
            text = title
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setPadding(0, dp(12), 0, dp(4))
        }

    private fun buildSwitch(key: String, label: String, default: Boolean): Switch {
        return Switch(this).apply {
            text = label
            isChecked = app.prefs.getBool(key, default)
            setPadding(0, dp(2), 0, dp(2))
            setOnCheckedChangeListener { _, checked ->
                app.prefs.setBool(key, checked)
            }
        }
    }

    private fun dp(v: Int): Int =
        (v * resources.displayMetrics.density).toInt()

    private fun loadMachines() {
        machineStatus.text = "loading…"
        exec.execute {
            val result = auth.machines()
            handler.post {
                result.fold(
                    onSuccess = { list ->
                        machines = list
                        machineStatus.text = "${list.size} machine(s) found"
                        if (list.isNotEmpty()) {
                            machinePicker.adapter = ArrayAdapter(
                                this,
                                android.R.layout.simple_spinner_item,
                                list.map { it.name + "  (${if (isOnline(it)) "online" else "offline"})" }
                            )
                        } else {
                            machinePicker.visibility = android.view.View.GONE
                        }
                    },
                    onFailure = { e ->
                        machineStatus.text = "error: ${e.message}"
                    }
                )
            }
        }
    }

    private fun isOnline(m: Machine): Boolean {
        if (m.lastSeenAt.isEmpty()) return false
        return try {
            val t = java.time.Instant.parse(m.lastSeenAt).toEpochMilli()
            System.currentTimeMillis() - t < 90_000
        } catch (_: Exception) { false }
    }

    private fun toggleMonitor() {
        if (Monitor.running) {
            val stopIntent = Intent(this, MonitorService::class.java).setAction("stop")
            startService(stopIntent)
            monitorBtn.text = "Start monitoring"
            monitorStatus.text = "stopping…"
        } else {
            val sel = machinePicker.selectedItemPosition
            if (sel < 0 || sel >= machines.size) {
                monitorStatus.text = "select a machine first"
                return
            }
            val m = machines[sel]
            app.prefs.set("machine_id", m.id)
            app.prefs.set("machine_name", m.name)
            val intent = Intent(this, MonitorService::class.java)
                .putExtra("machineId", m.id)
                .putExtra("machineName", m.name)
            startForegroundService(intent)
            monitorBtn.text = "Stop monitoring"
            monitorStatus.text = "starting…"
        }
    }

    private fun requestNotifPermission() {
        if (Build.VERSION.SDK_INT >= 33) {
            requestPermissions(
                arrayOf(android.Manifest.permission.POST_NOTIFICATIONS),
                1
            )
        } else {
            monitorStatus.text = "permission not required (pre-Android 13)"
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 1) {
            val granted = grantResults.isNotEmpty() &&
                grantResults[0] == PackageManager.PERMISSION_GRANTED
            monitorStatus.text = if (granted)
                "notification permission granted ✓"
            else "notification permission denied"
        }
    }

}
