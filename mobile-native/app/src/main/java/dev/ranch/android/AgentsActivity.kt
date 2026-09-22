package dev.ranch.android

import android.app.Activity
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
import android.widget.Switch
import android.widget.TextView
import org.json.JSONObject

/**
 * Phase 4 — Agents / pi manager.
 * `PiList` -> `PiListOk` (list of local pi sessions: title, cwd, active,
 * external, mtime). `PiMonitor{enabled}` toggles watching external pi
 * processes. Tapping a session "adopts" it: `SessionsCreate{kind:pi,
 * pi_session_file}` spawns a ranch session bound to that pi conversation,
 * which then appears in the live session list and can be opened in
 * [SessionActivity].
 */
class AgentsActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var list: LinearLayout
    private lateinit var status: TextView
    private lateinit var monitorSw: Switch
    private var monitorOn = false

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
            text = "Agents (pi)"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }
        bar.addView(back)
        bar.addView(title, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        monitorSw = Switch(this).apply {
            text = "Watch external pi sessions"
            setOnCheckedChangeListener { _, c ->
                monitorOn = c
                relay?.send(Term.piMonitor(c))
            }
        }
        root.addView(monitorSw)

        root.addView(Button(this).apply {
            text = "＋ New agent session"
            setPadding(0, dp(6), 0, dp(6))
            setOnClickListener { relay?.send(Term.sessionsCreate("pi")) }
        })

        status = TextView(this).apply {
            text = "loading…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(list) },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        setContentView(root)
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.piList())
    }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "PiListOk" -> {
                val sessions = f.optJSONArray("sessions")
                val n = sessions?.length() ?: 0
                status.text = "$n pi session(s)"
                list.removeAllViews()
                sessions?.let { a ->
                    for (i in 0 until a.length()) {
                        val s = Term.parsePiSession(a.getJSONObject(i))
                        list.addView(row(s))
                    }
                }
                if (n == 0) list.addView(TextView(this).apply {
                    text = "no local pi sessions"; setTextColor(0xFF6b7280.toInt())
                })
            }
            "PiMonitorOk" -> {
                monitorOn = f.optBoolean("enabled", false)
                monitorSw.isChecked = monitorOn
                status.text = "pi monitor ${if (monitorOn) "on" else "off"}"
            }
            "SessionsAck" -> {
                status.text = "adopted — see the session list on the home screen"
            }
        }
    }

    private fun row(s: Term.PiSession): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        val mark = if (s.active) "●" else "○"
        val ext = if (s.external) " [ext]" else ""
        wrap.addView(TextView(this).apply {
            text = "$mark ${s.title.ifEmpty { s.id.take(8) }}$ext"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(if (s.active) 0xFF7fd4ff.toInt() else 0xFFd1d5db.toInt())
        })
        wrap.addView(TextView(this).apply {
            text = s.path.ifEmpty { "?" }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF6b7280.toInt())
        })
        wrap.addView(Button(this).apply {
            text = if (s.active) "open in ranch" else "adopt into ranch"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(0, dp(4), 0, dp(4))
            setOnClickListener {
                relay?.send(Term.adoptPiSession(s.sessionFile))
                status.text = "adopting ${s.title.ifEmpty { s.id.take(8) }}…"
            }
        })
        return wrap
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@AgentsActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@AgentsActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
