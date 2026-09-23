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

/**
 * Phase 4 — Workflows / mule.
 * `WorkflowList` -> `WorkflowListOk` (WorkflowSummary[]: id, name,
 * description, is_async, updated_at). Tap -> `WorkflowGet` ->
 * `WorkflowGetOk` (workflow + steps[]). Row actions: Run
 * (`WorkflowRun` — spawns a new pane running the workflow) and Delete
 * (`WorkflowDelete`).
 */
class WorkflowsActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var list: LinearLayout
    private lateinit var status: TextView
    private var selectedId = ""

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
            text = "Workflows"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL; setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "loading…"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(0, dp(4), 0, dp(4))
        }
        root.addView(status)

        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(list) },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        r.send(Term.workflowList())
    }

    override fun onDestroy() {
        relay?.removeSink(sink); handler.removeCallbacksAndMessages(null); super.onDestroy()
    }

    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "WorkflowListOk" -> {
                val arr = f.optJSONArray("workflows")
                val n = arr?.length() ?: 0
                status.text = "$n workflow(s)"
                list.removeAllViews()
                arr?.let { a ->
                    for (i in 0 until a.length()) {
                        val w = a.getJSONObject(i)
                        list.addView(workflowRow(w.optString("id"), w.optString("name"),
                            w.optString("description"), w.optInt("updated_at", 0)))
                    }
                }
                if (n == 0) list.addView(TextView(this).apply {
                    text = "no workflows"; setTextColor(0xFF6b7280.toInt())
                })
            }
            "WorkflowGetOk" -> {
                val wf = f.optJSONObject("workflow") ?: return
                val steps = f.optJSONArray("steps")
                val n = steps?.length() ?: 0
                status.text = "${wf.optString("name")}: $n step(s)"
                // append a detail block under the list
                val detail = LinearLayout(this).apply {
                    orientation = LinearLayout.VERTICAL
                    setPadding(dp(8), dp(4), dp(8), dp(4))
                }
                detail.addView(TextView(this).apply {
                    text = "steps:"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setTextColor(0xFF7fd4ff.toInt())
                })
                steps?.let { a ->
                    for (i in 0 until a.length()) {
                        val s = a.getJSONObject(i)
                        detail.addView(TextView(this).apply {
                            text = "  ${s.optInt("step_order")}. ${s.optString("type")}" +
                                    (if (s.optString("agent_id").isNotEmpty()) " → ${s.optString("agent_id")}" else "")
                            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setTextColor(0xFFd1d5db.toInt())
                        })
                    }
                }
                list.addView(detail)
            }
            "WorkflowDeleteOk" -> {
                status.text = "deleted"
            }
        }
    }

    private fun workflowRow(id: String, name: String, desc: String, updated: Int): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        wrap.addView(TextView(this).apply {
            text = name.ifEmpty { id }; setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
        })
        if (desc.isNotEmpty()) wrap.addView(TextView(this).apply {
            text = desc; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setTextColor(0xFF9aa0a6.toInt())
        })
        val actions = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        actions.addView(Button(this).apply {
            text = "Details"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setPadding(dp(6), dp(3), dp(6), dp(3))
            setOnClickListener { relay?.send(Term.workflowGet(id)) }
        })
        actions.addView(Button(this).apply {
            text = "Run"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setPadding(dp(6), dp(3), dp(6), dp(3))
            setOnClickListener { relay?.send(Term.workflowRun(id)); status.text = "running '$name'…" }
        })
        actions.addView(Button(this).apply {
            text = "Delete"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setPadding(dp(6), dp(3), dp(6), dp(3))
            setOnClickListener { relay?.send(Term.workflowDelete(id)) }
        })
        wrap.addView(actions)
        return wrap
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()
    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@WorkflowsActivity).apply { text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f) })
            addView(Button(this@WorkflowsActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
