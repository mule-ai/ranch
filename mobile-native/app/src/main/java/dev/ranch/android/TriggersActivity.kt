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
import android.widget.TextView
import org.json.JSONObject

/**
 * Phase 4 — Triggers.
 * `TriggerList` -> `TriggerListOk` (triggers as raw JSON values: name,
 * workflow, kind, cron/event spec, enabled). Row actions: Run
 * (`TriggerRun`), Delete (`TriggerDelete`). New trigger via `TriggerPut`
 * with a minimal cron form (name + cron + workflow + enabled).
 * `TriggerFired` broadcast shows a live "fired" line.
 */
class TriggersActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var list: LinearLayout
    private lateinit var status: TextView
    private lateinit var fired: TextView

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
            text = "Triggers"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL; setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        // new-trigger form
        root.addView(TextView(this).apply {
            text = "New trigger"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFF7fd4ff.toInt()); setPadding(0, dp(4), 0, dp(4))
        })
        val name = EditText(this).apply { hint = "name" }
        val cron = EditText(this).apply { hint = "cron expr, e.g. 0 9 * * *" }
        val wf = EditText(this).apply { hint = "workflow id" }
        root.addView(name); root.addView(cron); root.addView(wf)
        root.addView(Button(this).apply {
            text = "Create trigger (cron)"
            setPadding(0, dp(5), 0, dp(5))
            setOnClickListener {
                val n = name.text.toString().trim()
                val c = cron.text.toString().trim()
                val w = wf.text.toString().trim()
                if (n.isEmpty() || c.isEmpty()) { status.text = "name + cron required"; return@setOnClickListener }
                val trigger = JSONObject()
                    .put("name", n)
                    .put("kind", "cron")
                    .put("cron", c)
                    .put("enabled", true)
                if (w.isNotEmpty()) trigger.put("workflow", w)
                relay?.send(JSONObject().put("t", "TriggerPut").put("req_id", "tp-" + Term.newId()).put("trigger", trigger))
                status.text = "creating '$n'…"
            }
        })

        status = TextView(this).apply {
            text = "loading…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(6), 0, dp(2))
        }
        root.addView(status)
        fired = TextView(this).apply {
            text = ""; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF4ade80.toInt())
        }
        root.addView(fired)

        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(list) },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.triggerList())
    }

    override fun onDestroy() {
        relay?.removeSink(sink); handler.removeCallbacksAndMessages(null); super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "TriggerListOk" -> {
                val arr = f.optJSONArray("triggers")
                val n = arr?.length() ?: 0
                status.text = "$n trigger(s)"
                list.removeAllViews()
                arr?.let { a ->
                    for (i in 0 until a.length()) {
                        val t = a.optJSONObject(i) ?: continue
                        list.addView(triggerRow(t))
                    }
                }
                if (n == 0) list.addView(TextView(this).apply {
                    text = "no triggers"; setTextColor(0xFF6b7280.toInt())
                })
            }
            "TriggerFired" -> {
                val name = f.optString("trigger")
                val job = f.optString("job")
                fired.text = "⚡ $name fired → $job"
            }
        }
    }

    private fun triggerRow(t: JSONObject): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        val id = t.optString("id").ifEmpty { t.optString("name") }
        val name = t.optString("name", id)
        val kind = t.optString("kind", "?")
        val enabled = if (t.has("enabled")) t.optBoolean("enabled") else true
        val wf = t.optString("workflow")
        val cron = t.optString("cron")
        wrap.addView(TextView(this).apply {
            text = (if (enabled) "●" else "○") + " " + name
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(if (enabled) 0xFF7fd4ff.toInt() else 0xFF6b7280.toInt())
        })
        wrap.addView(TextView(this).apply {
            text = listOf(kind, cron.ifEmpty { null }, wf.ifEmpty { null }).filterNotNull().joinToString(" · ").ifEmpty { "—" }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setTextColor(0xFF9aa0a6.toInt())
        })
        val actions = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        actions.addView(Button(this).apply {
            text = "Run"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setPadding(dp(6), dp(3), dp(6), dp(3))
            setOnClickListener { relay?.send(Term.triggerRun(id)); status.text = "fired '$name'" }
        })
        actions.addView(Button(this).apply {
            text = "Delete"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setPadding(dp(6), dp(3), dp(6), dp(3))
            setOnClickListener { relay?.send(Term.triggerDelete(id)); status.text = "deleted '$name'" }
        })
        wrap.addView(actions)
        return wrap
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()
    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@TriggersActivity).apply { text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f) })
            addView(Button(this@TriggersActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
