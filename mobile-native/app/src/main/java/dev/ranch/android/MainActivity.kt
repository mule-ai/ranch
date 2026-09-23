package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors

/**
 * Staged flow, mirroring the RN app (login → machine list → session list):
 *
 *   1. **Login** — email/password + Google (deep-link return).
 *   2. **Machine pick** — big tappable rows (name + online dot); tapping a
 *      machine starts the foreground monitor service immediately.
 *   3. **Monitor home** — live session list (the primary surface), new
 *      session buttons, and one entry point into [SettingsActivity] for
 *      notification toggles / tools / diagnostics.
 */
class MainActivity : Activity() {

    private val app get() = application as App

    // lazy: field initializers run at construction, before Activity.attach(),
    // where getApplication() is still null (NPE = crash on launch)
    private val auth by lazy { Auth(app.prefs) }
    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()

    private lateinit var stateBox: LinearLayout
    private lateinit var bannerBox: LinearLayout
    private var sessionsBox: LinearLayout? = null
    private var homeStatus: TextView? = null
    private var monitorWanted = false
    private val machines = mutableListOf<Machine>()
    private var update: Version.Update? = null

    private val refreshRunnable = object : Runnable {
        override fun run() {
            refreshSessions()
            handler.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        monitorWanted = Monitor.running
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(16), dp(16), dp(16), dp(16))
        }
        setContentView(ScrollView(this).apply { addView(root) })
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        addCrashRow(root)
        bannerBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(bannerBox)
        stateBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(stateBox)
        handleAuthIntent(intent)
        renderState()
    }

    override fun onNewIntent(i: Intent) {
        super.onNewIntent(i)
        handleAuthIntent(i)
        renderState()
    }

    override fun onResume() {
        super.onResume()
        renderState()
        renderBanner()
        // update check (silent on failure / dev builds)
        exec.execute {
            val u = Version.checkUpdate()
            handler.post {
                val changed = u?.latest != update?.latest
                update = u
                if (changed) renderBanner()
            }
        }
        handler.postDelayed(refreshRunnable, 1000)
    }

    override fun onPause() {
        handler.removeCallbacks(refreshRunnable)
        super.onPause()
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        exec.shutdownNow()
        super.onDestroy()
    }

    // ---- state machine ----
    private fun renderState() {
        if (!::stateBox.isInitialized) return
        stateBox.removeAllViews()
        sessionsBox = null
        homeStatus = null
        when {
            !auth.isLoggedIn() -> renderLogin()
            !monitorWanted && !Monitor.running -> renderMachinePick()
            else -> renderMonitorHome()
        }
    }

    // ---- 1. login ----
    private fun renderLogin() {
        stateBox.addView(TextView(this).apply {
            text = "🤠 Ranch"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 30f)
            setTextColor(0xFFE5E5E5.toInt())
            gravity = Gravity.CENTER
            setPadding(0, dp(48), 0, dp(4))
        })
        stateBox.addView(TextView(this).apply {
            text = "your agents, in your pocket"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFF6B7280.toInt())
            gravity = Gravity.CENTER
            setPadding(0, 0, 0, dp(32))
        })
        val email = EditText(this).apply {
            hint = "email"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        }
        val pw = EditText(this).apply {
            hint = "password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        }
        val status = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }
        stateBox.addView(email)
        stateBox.addView(pw)
        stateBox.addView(Button(this).apply {
            text = "Sign in"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setPadding(0, dp(10), 0, dp(10))
            setOnClickListener {
                status.text = "signing in…"
                exec.execute {
                    val err = auth.login(email.text.toString(), pw.text.toString())
                    handler.post {
                        status.text = if (err == null) "signed in ✓" else "error: $err"
                        if (err == null) renderState()
                    }
                }
            }
        })
        stateBox.addView(Button(this).apply {
            text = "Sign in with Google"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(auth.googleAuthorizeUrl())))
                    status.text = "completing sign-in in browser…"
                } catch (e: Exception) { status.text = "no browser: ${e.message}" }
            }
        })
        stateBox.addView(status)
    }

    // ---- 2. machine pick ----
    private fun renderMachinePick() {
        stateBox.addView(TextView(this).apply {
            text = "Pick a machine"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 22f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, dp(24), 0, dp(4))
        })
        stateBox.addView(TextView(this).apply {
            text = "tapping a machine starts monitoring it"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setPadding(0, 0, 0, dp(12))
        })
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        stateBox.addView(list)
        val status = TextView(this).apply {
            text = "loading machines…"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }
        stateBox.addView(status)
        stateBox.addView(Button(this).apply {
            text = "sign out"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setOnClickListener { auth.logout(); renderState() }
        })
        loadMachines(list, status)
    }

    private fun loadMachines(list: LinearLayout, status: TextView) {
        exec.execute {
            auth.machines().fold(
                onSuccess = { items -> handler.post {
                    machines.clear(); machines.addAll(items)
                    list.removeAllViews()
                    for (m in items) list.addView(machineRow(m))
                    status.text = if (items.isEmpty())
                        "no machines — run 'ranch register' on a daemon host"
                    else "${items.size} machine(s)"
                } },
                onFailure = { e -> handler.post { status.text = "error: ${e.message}" } }
            )
        }
    }

    private fun machineRow(m: Machine): View {
        val online = isOnline(m)
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(14), dp(16), dp(14))
            background = GradientDrawable().apply {
                cornerRadius = dp(12).toFloat()
                setColor(0xFF1C2127.toInt())
            }
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                LinearLayout.LayoutParams.WRAP_CONTENT
            ).apply { setMargins(0, dp(6), 0, dp(6)) }
            setOnClickListener { selectMachine(m) }
        }
        row.addView(TextView(this).apply {
            text = (if (online) "● " else "○ ") + m.name
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 17f)
            setTextColor(if (online) 0xFF4ADE80.toInt() else 0xFF9AA0A6.toInt())
        })
        row.addView(TextView(this).apply {
            text = if (online) "online — tap to monitor"
            else "offline${if (m.lastSeenAt.isNotEmpty()) " · last seen ${m.lastSeenAt}" else ""}"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF6B7280.toInt())
            setPadding(0, dp(2), 0, 0)
        })
        return row
    }

    private fun selectMachine(m: Machine) {
        app.prefs.set("machine_id", m.id)
        app.prefs.set("machine_name", m.name)
        monitorWanted = true
        startForegroundService(Intent(this, MonitorService::class.java)
            .putExtra("machineId", m.id)
            .putExtra("machineName", m.name))
        renderState()
    }

    // ---- 3. monitor home ----
    private fun renderMonitorHome() {
        val head = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
        head.addView(TextView(this).apply {
            text = "🤠 ${Monitor.machineName.ifEmpty { "monitor" }}"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(0, dp(8), 0, dp(8))
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        head.addView(Button(this).apply {
            text = "Stop"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setOnClickListener {
                monitorWanted = false
                startService(Intent(this@MainActivity, MonitorService::class.java).setAction("stop"))
                renderState()
            }
        })
        stateBox.addView(head)

        homeStatus = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
            setPadding(0, 0, 0, dp(4))
        }
        stateBox.addView(homeStatus)

        stateBox.addView(TextView(this).apply {
            text = "Sessions"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7FD4FF.toInt())
            setPadding(0, dp(12), 0, dp(4))
        })
        val newSessionRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for ((label, kind) in listOf("+ shell" to "shell", "+ agent" to "pi")) {
            newSessionRow.addView(Button(this).apply {
                text = label
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setPadding(dp(8), dp(8), dp(8), dp(8))
                layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
                setOnClickListener {
                    Monitor.relay?.createSession(kind)
                    homeStatus?.text = "creating $kind session…"
                }
            })
        }
        stateBox.addView(newSessionRow)
        val box = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        sessionsBox = box
        stateBox.addView(box)
        refreshSessions()

        stateBox.addView(Button(this).apply {
            text = "⚙ settings, tools & diagnostics"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(0, dp(16), 0, dp(8))
            setOnClickListener { startActivity(Intent(this@MainActivity, SettingsActivity::class.java)) }
        })
    }

    private fun refreshSessions() {
        val box = sessionsBox ?: return
        val sessions = Monitor.sessions
        homeStatus?.text = when {
            Monitor.status != "open" && Monitor.status != "monitoring" && !Monitor.running -> "disconnected"
            sessions.isEmpty() -> "no sessions yet"
            else -> "${sessions.size} session(s) · ${Monitor.status}"
        }
        box.removeAllViews()
        for (s in sessions) {
            box.addView(Button(this).apply {
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
            })
        }
    }

    // ---- shared ----
    /** Out-of-date banner + daemon-version note (port of the RN banner). */
    private fun renderBanner() {
        if (!::bannerBox.isInitialized) return
        bannerBox.removeAllViews()
        update?.let { u ->
            bannerBox.addView(Button(this).apply {
                text = "⬆ update available — download ${u.latest}"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setPadding(dp(10), dp(10), dp(10), dp(10))
                layoutParams = LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT,
                    LinearLayout.LayoutParams.WRAP_CONTENT
                ).apply { setMargins(0, dp(4), 0, dp(4)) }
                setOnClickListener {
                    try {
                        startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(u.apkUrl)))
                    } catch (e: Exception) {
                        Toast.makeText(this@MainActivity, "no browser: ${e.message}", Toast.LENGTH_LONG).show()
                    }
                }
            })
        }
        val dv = Monitor.daemonVersion
        if (update != null && dv.isNotEmpty() && dv != "dev" && dv != update?.latest) {
            bannerBox.addView(TextView(this).apply {
                text = "daemon runs $dv — latest is ${update?.latest}; update the daemon (\"ranch upgrade\" or reinstall)"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFFF59E0B.toInt())
                setPadding(dp(4), dp(2), dp(4), dp(6))
            })
        }
    }

    private fun addCrashRow(root: LinearLayout) {
        val crashFile = java.io.File(filesDir, "crash.txt")
        if (!crashFile.exists()) return
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

    private fun handleAuthIntent(i: Intent?) {
        val data = i?.data ?: return
        if (data.scheme != "ranch" || data.host != "auth-callback") return
        val err = auth.applyOAuthFragment(data.fragment)
        if (err == null) renderState()
        else Toast.makeText(this, "google sign-in failed: $err", Toast.LENGTH_LONG).show()
    }

    private fun isOnline(m: Machine): Boolean {
        if (m.lastSeenAt.isEmpty()) return false
        return try {
            val t = java.time.Instant.parse(m.lastSeenAt).toEpochMilli()
            System.currentTimeMillis() - t < 90_000
        } catch (_: Exception) { false }
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()
}
