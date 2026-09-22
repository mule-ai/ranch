package dev.ranch.android

import android.app.Activity
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
import java.time.Instant
import java.util.concurrent.Executors

/**
 * Phase 4 — Machines.
 * Lists the user's machines (REST `machines_info` via [Auth], same source
 * as the home screen) with live online status. The **Upgrade** button sends
 * the `Upgrade` frame on the monitored machine's channel, hot-upgrading the
 * running daemon in place (sessions/panes survive — see AGENTS.md "Key
 * invariants & gotchas").
 */
class MachinesActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var list: LinearLayout
    private lateinit var status: TextView
    private var machines: List<Machine> = emptyList()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val r = Monitor.relay
        relay = r

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(12), dp(12), dp(12), dp(12))
        }
        val bar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        bar.addView(Button(this).apply { text = "←"; setOnClickListener { finish() } })
        bar.addView(TextView(this).apply {
            text = "Machines"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL; setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "loading…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt())
        }
        root.addView(status)

        // Upgrade the monitored machine's daemon (hot, sessions survive)
        root.addView(Button(this).apply {
            text = "⬆ Upgrade monitored machine's daemon"
            setPadding(0, dp(8), 0, dp(8))
            setOnClickListener {
                val rr = relay
                if (rr == null) { status.text = "start monitoring first"; return@setOnClickListener }
                status.text = "sending Upgrade — daemon re-execs in place…"
                rr.send(Term.upgrade())
                handler.postDelayed({
                    status.text = "upgrade sent (check the daemon log on the host)"
                }, 3000)
            }
        })

        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(list) },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        setContentView(root)
        // monitor status frames (Meta / Upgrade ack) — light: just status text
        if (r != null) {
            sink = { f -> handler.post {
                if (f.optString("t") == "Error" && f.optString("req_id").startsWith("upg"))
                    status.text = "upgrade error: ${f.optString("message")}"
            } }
            r.addSink(sink)
        }
        loadMachines()
    }

    override fun onDestroy() {
        if (::sink.isInitialized) relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        exec.shutdownNow()
        super.onDestroy()
    }

    private fun loadMachines() {
        status.text = "loading machines…"
        val auth = Auth((application as App).prefs)
        exec.execute {
            auth.machines().fold(
                onSuccess = { items -> handler.post {
                    machines = items
                    status.text = "${items.size} machine(s)"
                    list.removeAllViews()
                    for (m in items) list.addView(machineRow(m))
                    if (list.childCount == 0) list.addView(TextView(this).apply {
                        text = "no machines"; setTextColor(0xFF6b7280.toInt())
                    })
                } },
                onFailure = { e -> handler.post { status.text = "error: ${e.message}" } }
            )
        }
    }

    private fun machineRow(m: Machine): LinearLayout {
        val online = isOnline(m)
        val monitored = m.id == Monitor.machineId
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        wrap.addView(TextView(this).apply {
            text = (if (online) "●" else "○") + " " + m.name +
                (if (monitored) "  (monitored)" else "")
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(if (online) 0xFF4ade80.toInt() else 0xFF6b7280.toInt())
        })
        val last = m.lastSeenAt
        if (last.isNotEmpty()) wrap.addView(TextView(this).apply {
            text = "last seen " + last
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setTextColor(0xFF9aa0a6.toInt())
        })
        return wrap
    }

    private fun isOnline(m: Machine): Boolean {
        if (m.lastSeenAt.isEmpty()) return false
        return try {
            val t = Instant.parse(m.lastSeenAt).toEpochMilli()
            System.currentTimeMillis() - t < 90_000
        } catch (_: Exception) { false }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()
    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@MachinesActivity).apply { text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f) })
            addView(Button(this@MachinesActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
