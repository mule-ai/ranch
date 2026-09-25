package dev.ranch.android

import android.app.Activity
import android.content.Context
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.text.Spannable
import android.text.SpannableStringBuilder
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.graphics.Typeface
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.HorizontalScrollView
import android.widget.PopupWindow
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject

/**
 * Session/terminal screen (Phase 3 full):
 * - PTY panes: TerminalView + sentinel-EditText input + special keys + predictive echo
 * - Agent panes: chat list with markdown, model bar + picker, context readout,
 *   agent-ask card, chat scrollback paging
 * - PTY scrollback button
 * - Pane tabs, PaneSelect, seq-gap resync, Resize
 */
class SessionActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private var sessionId = ""
    private var sessionName = ""

    // pane state
    private data class Pane(
        var kind: String = "pty",
        var cols: Int = 80,
        var rows: Int = 24,
        var lines: List<String> = emptyList(),
        var cursor: Triple<Int, Int, Boolean>? = null,
        var chat: MutableList<Term.ChatMsg> = mutableListOf(),
        var chatHasMore: Boolean = false,
        var model: String = "",
        var context: String = "",
        var seq: Long = 0,
        /// true while the pane's agent has an in-flight turn (drives the
        /// ⏹ stop button); flipped by `meta { kind: "agent" }` frames.
        var agentWorking: Boolean = false,
    )
    private val panes = LinkedHashMap<String, Pane>()
    private var activePane = ""
    /// pane requested by a notification deep link (open_pane extra)
    private var preferredPane = ""
    private val lastSeq = LinkedHashMap<String, Long>()
    private var uiKind = ""
    private var agentStatus = ""

    // agent ask
    private var pendingAsk: Term.AgentAsk? = null
    private var askAnswerNote: String? = null
    private var askSelected = mutableSetOf<Int>()
    private var askEdit: EditText? = null
    private val askChoiceBtns = mutableListOf<Button>()
    private lateinit var askContainer: LinearLayout

    // model picker
    private val modelOpts = LinkedHashMap<String, List<Term.ModelChoice>>()
    private var modelPw: PopupWindow? = null
    private var modelPickerOpen = false

    // chat scrollback
    private var chatHistReqId: String? = null
    private var chatScrollAnchor: Pair<Int, Int>? = null
    private var renderedSeq = 0L   // highest chat seq currently on screen

    // compaction + queued messages: the daemon confirms a finished
    // compaction with a context Meta ("compacted → N est. tokens") or an
    // Error{req_id} on failure — there is no "compacting" broadcast, so
    // progress is tracked client-side
    private var compacting = false
    private var compactingPane = ""
    private var compactingSince = 0L
    private var compactReqId: String? = null
    private val queued = mutableListOf<Queued>()
    private var queuedNote: String? = null
    private lateinit var queueContainer: LinearLayout

    private data class Queued(val pane: String, val text: String)

    private val compactTimeoutRun = Runnable {
        if (compacting) finishCompact("compaction timed out — sending queued messages")
    }

    // ---- compaction persistence (survives back-swipe + process kill) ----
    // Keyed per session: {pane, since, queued:[{pane,text}]}. Restored on
    // create; reconciled against the daemon's cached context readout when
    // the Snapshot lands (compaction that finished while the app was away
    // shows up as "compacted → …" and flushes the queue).
    private fun compactKey(): String = "compact.$sessionId"

    private fun prefs() = (application as App).prefs

    private fun persistCompaction() {
        val o = JSONObject()
            .put("pane", compactingPane)
            .put("since", compactingSince)
        val arr = org.json.JSONArray()
        for (q in queued) arr.put(JSONObject().put("pane", q.pane).put("text", q.text))
        o.put("queued", arr)
        prefs().set(compactKey(), o.toString())
    }

    private fun clearPersistedCompaction() {
        prefs().set(compactKey(), "")
    }

    private fun restoreCompaction() {
        val raw = prefs().get(compactKey(), "")
        if (raw.isEmpty()) return
        try {
            val o = JSONObject(raw)
            val pane = o.optString("pane")
            compactingSince = o.optLong("since", 0)
            val arr = o.optJSONArray("queued")
            if (arr != null) {
                for (i in 0 until arr.length()) {
                    val q = arr.getJSONObject(i)
                    queued.add(Queued(q.optString("pane"), q.optString("text")))
                }
            }
            if (pane.isEmpty() && queued.isEmpty()) {
                clearPersistedCompaction(); return
            }
            compactingPane = pane
            compacting = pane.isNotEmpty()
            // compaction that outlived the app for >10 min is stale
            if (compacting &&
                (compactingSince <= 0 ||
                 System.currentTimeMillis() - compactingSince > 600_000)
            ) {
                finishCompact("compaction timed out — sending queued messages")
            }
        } catch (_: Exception) {
            clearPersistedCompaction()
        }
    }

    // views
    private lateinit var titleView: TextView
    private lateinit var statusView: TextView
    private lateinit var paneTabs: LinearLayout
    private lateinit var content: LinearLayout
    private var term: TerminalView? = null
    private lateinit var chatScroll: ScrollView
    private lateinit var chatBox: LinearLayout
    private lateinit var inputArea: LinearLayout
    private lateinit var hiddenEdit: EditText
    private lateinit var chatEdit: EditText
    private var stopBtn: Button? = null
    private lateinit var modelBar: LinearLayout
    private lateinit var modelChip: TextView
    private lateinit var contextLabel: TextView
    private lateinit var sink: (JSONObject) -> Unit

    // ---- lifecycle ----
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        sessionName = intent.getStringExtra("sessionName") ?: ""
        sessionId = intent.getStringExtra("sessionId") ?: ""
        val r = Monitor.relay
        if (r == null || sessionId.isEmpty()) {
            setContentView(errorView("Monitor is not running.\nStart monitoring first, then open a session."))
            return
        }
        relay = r

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
        }

        // top bar
        val top = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val back = Button(this).apply {
            text = "←"; minWidth = 0; setPadding(0,0,0,0)
            setOnClickListener { finish() }
        }
        titleView = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0); text = sessionName.ifEmpty { sessionId.take(8) }
        }
        statusView = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF7fd4ff.toInt()); gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, dp(8), 0); text = ""
        }
        top.addView(back)
        top.addView(titleView, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        top.addView(Button(this).apply {
            text = "✎"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener { renameSession() }
        })
        top.addView(Button(this).apply {
            text = "✕"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setPadding(dp(6), 0, dp(6), 0)
            setTextColor(0xFFef4444.toInt())
            setOnClickListener {
                relay?.send(Term.sessionsKill(sessionId))
                statusView.text = "killing…"
                handler.postDelayed({ finish() }, 800)
            }
        })
        top.addView(statusView)
        root.addView(top)

        paneTabs = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            visibility = View.GONE
            setPadding(dp(8), 0, dp(8), 0)
        }
        root.addView(paneTabs)

        // model bar (visible for chat panes)
        modelBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            visibility = View.GONE
            setPadding(dp(8), dp(4), dp(8), dp(4))
        }
        modelChip = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF7fd4ff.toInt())
            text = "model: ?"
            setPadding(dp(8), dp(4), dp(8), dp(4))
            setBackgroundColor(0xFF1b2126.toInt())
        }
        contextLabel = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt())
            setPadding(dp(8), 0, 0, 0)
        }
        modelBar.addView(modelChip)
        modelBar.addView(contextLabel)
        root.addView(modelBar)

        // content
        content = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(content, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // ask card container (above input)
        askContainer = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            setPadding(dp(8), dp(4), dp(8), dp(4))
        }
        root.addView(askContainer)

        // compaction banner + queued-message rows (above input)
        queueContainer = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
            setPadding(dp(8), dp(2), dp(8), dp(2))
        }
        root.addView(queueContainer)

        // input area
        inputArea = LinearLayout(this)
        root.addView(inputArea, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        setContentView(root)

        // restore compaction state + queued messages that outlived the
        // screen (back-swipe, backgrounding, process kill)
        restoreCompaction()
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))

        // keyboard open/close resizes the window — keep the newest chat
        // message visible, but only if the user was already at the bottom
        content.addOnLayoutChangeListener { _, _, top, _, bottom, _, oldTop, _, oldBottom ->
            if ((top != oldTop || bottom != oldBottom) && uiKind == "forge-chat") {
                chatScroll.post { chatStickBottom() }
            }
        }

        // subscribe + attach
        sink = { frame -> onFrame(frame) }
        r.addSink(sink)
        r.attach(sessionId)
    }

    override fun onResume() { super.onResume(); term?.focused = true }
    override fun onPause() { term?.focused = false; super.onPause() }
    override fun onDestroy() {
        relay?.let { it.removeSink(sink); it.detach() }
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    // ---- frame dispatch (WS thread -> main) ----
    private fun onFrame(f: JSONObject) {
        if (f.has("session") && f.optString("session") != sessionId) return
        handler.post { handle(f) }
    }

    private fun handle(f: JSONObject) {
        when (f.optString("t")) {
            "Snapshot" -> onSnapshot(f)
            "Update" -> onUpdate(f)
            "Chat" -> onChat(f)
            "Meta" -> onMeta(f)
            "AgentAskRequest" -> onAgentAsk(f)
            "AgentAskAnswer" -> onAgentAskAnswer(f)
            "ModelListOk" -> onModelListOk(f)
            "ChatHistoryOk" -> onChatHistoryOk(f)
            "Scrollback" -> onScrollback(f)
            "Error" -> {
                // request-scoped errors (req_id set) are only meaningful
                // to the client that sent the request — but unsolicited
                // errors (req_id absent: upgrade denied, hot-upgrade
                // failure, …) must flash, same as the TUI status bar
                val rid = f.optString("req_id")
                when {
                    rid == compactReqId -> {
                        statusView.text = "compaction failed: ${f.optString("message")}"
                        finishCompact()
                    }
                    rid.isEmpty() -> statusView.text = "err: ${f.optString("message")}"
                }
            }
        }
    }

    private fun onSnapshot(f: JSONObject) {
        activePane = f.optString("active_pane")
        val arr = f.optJSONArray("panes") ?: return
        val next = LinkedHashMap<String, Pane>()
        for (i in 0 until arr.length()) {
            val p = arr.getJSONObject(i)
            val id = p.getString("id")
            val kind = p.optString("kind", "pty")
            val lines = p.optJSONArray("lines")
                ?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList()
            val cursor = p.optJSONObject("cursor")?.let { c ->
                Triple(c.optInt("x"), c.optInt("y"), c.optBoolean("visible", true))
            }
            val chat = p.optJSONArray("chat")
                ?.let { a -> (0 until a.length()).map { Term.parseChatMsg(a.getJSONObject(it)) }.toMutableList() }
                ?: mutableListOf()
            next[id] = Pane(
                kind = kind,
                cols = p.optInt("cols", 80),
                rows = p.optInt("rows", 24),
                lines = lines, cursor = cursor, chat = chat,
                chatHasMore = chat.size >= Term.CHAT_TAIL,
                model = p.optString("model"),
                context = p.optString("context"),
                seq = p.optLong("seq", 0),
            )
            if (p.optLong("seq", 0) > 0) lastSeq[id] = p.optLong("seq")
        }
        // notification deep link: prefer the pane that fired it over the
        // session's daemon-side active pane (honored once, on the first
        // snapshot — later snapshots follow the daemon again)
        if (preferredPane.isNotEmpty() && next.containsKey(preferredPane)) {
            activePane = preferredPane
            preferredPane = ""
        }
        panes.clear(); panes.putAll(next)
        rebuildTabs()
        renderActive()
        updateModelBar()
        // compaction completed while the app was away? the daemon caches the
        // last context readout on the pane, so the Snapshot tells us
        if (compacting && panes[compactingPane]?.context?.startsWith("compacted") == true) {
            finishCompact()
        }
        if (uiKind == "pty") sendResize()
    }

    private fun onUpdate(f: JSONObject) {
        val paneId = f.optString("pane")
        val pane = panes[paneId] ?: return
        val seq = f.optLong("seq", 0)
        val prev = lastSeq[paneId] ?: 0
        if (prev > 0 && seq != prev + 1L) {
            if (seq <= prev) return
            lastSeq.remove(paneId)
            relay?.attach(sessionId, null)
            return
        }
        lastSeq[paneId] = seq
        pane.cols = f.optInt("cols", pane.cols)
        pane.rows = f.optInt("rows", pane.rows)
        val newLines = ArrayList<String>(pane.lines)
        val upd = f.optJSONArray("rows_upd")
        if (upd != null) {
            for (i in 0 until upd.length()) {
                val pair = upd.getJSONArray(i)
                val y = pair.optInt(0); val text = pair.optString(1)
                while (newLines.size <= y) newLines.add("")
                newLines[y] = text
            }
        }
        pane.lines = newLines
        val c = f.optJSONObject("cursor")?.let {
            Triple(it.optInt("x"), it.optInt("y"), it.optBoolean("visible", true))
        }
        if (c != null) pane.cursor = c
        if (paneId == activePane && uiKind == "pty") {
            term?.applyUpdate(
                upd?.let { a -> (0 until a.length()).map { i ->
                    val p = a.getJSONArray(i); p.optInt(0) to p.optString(1)
                } } ?: emptyList(), c
            )
        }
    }

    private fun onChat(f: JSONObject) {
        val paneId = f.optString("pane")
        val pane = panes[paneId] ?: return
        val msgs = f.optJSONArray("msgs")?.let { a ->
            (0 until a.length()).map { Term.parseChatMsg(a.getJSONObject(it)) }
        } ?: return
        if (f.optBoolean("reset")) {
            pane.chat = msgs.toMutableList()
            pane.chatHasMore = false
        } else {
            val last = pane.chat.lastOrNull()?.seq ?: 0L
            for (m in msgs) if (m.seq > last) pane.chat.add(m)
        }
        // a user row landing means the agent took the message — show the
        // stop button even before the `meta working` frame arrives
        if (pane.chat.lastOrNull()?.role == "user") pane.agentWorking = true
        if (paneId == activePane && uiKind == "forge-chat") {
            renderChat(pane, false)
            updateStopButton()
        }
    }

    private fun onMeta(f: JSONObject) {
        when (f.optString("kind")) {
            "agent" -> {
                val st = f.optString("status", "")
                val pane = f.optString("pane")
                // compaction is a machine-wide lifecycle state: the daemon
                // broadcasts "compacting" when a ChatCompact lands (from any
                // client) and "idle" when it completes
                if (st == "compacting" && pane.isNotEmpty()) {
                    if (!compacting) compactingSince = System.currentTimeMillis()
                    compacting = true
                    compactingPane = pane
                    persistCompaction()
                    handler.removeCallbacks(compactTimeoutRun)
                    handler.postDelayed(compactTimeoutRun, 600_000)
                    renderQueue()
                    if (pane == activePane) {
                        statusView.text = "🗜 compacting…"
                        updateModelBar()
                    }
                    return
                }
                if (compacting && st == "idle" && pane == compactingPane) {
                    // completion signal — the context readout / Error frame
                    // usually lands first; this catches any straggler
                    finishCompact()
                }
                agentStatus = st
                panes[pane]?.agentWorking = (st == "working")
                statusView.text = when (st) {
                    "working" -> "● working"
                    "idle" -> "● idle"
                    else -> ""
                }
                if (pane == activePane && uiKind == "forge-chat") updateStopButton()
            }
            "model" -> {
                val p = f.optString("pane")
                panes[p]?.let { pane ->
                    if (pane.kind == "forge-chat") {
                        pane.model = f.optString("status")
                        if (p == activePane && uiKind == "forge-chat") updateModelBar()
                    }
                }
            }
            "context" -> {
                val p = f.optString("pane")
                panes[p]?.let { pane ->
                    pane.context = f.optString("status")
                    if (p == activePane && uiKind == "forge-chat") updateModelBar()
                    // the daemon confirms a finished compaction with a
                    // context readout ("compacted → N est. tokens")
                    if (compacting && p == compactingPane &&
                        pane.context.startsWith("compacted")) {
                        finishCompact()
                    }
                }
            }
            "exited" -> {
                // nothing to flush into a dead session
                compacting = false
                compactingPane = ""
                queued.clear()
                clearPersistedCompaction()
                statusView.text = "session ended"
                handler.postDelayed({ finish() }, 1500)
            }
        }
    }

    // ---- Agent Ask ----
    private fun onAgentAsk(f: JSONObject) {
        val ask = Term.parseAgentAsk(f)
        if (ask.session != sessionId) return
        pendingAsk = ask
        askAnswerNote = null
        askSelected = if (ask.suggested != null) mutableSetOf(ask.suggested) else mutableSetOf()
        buildAskCard()
    }

    private fun onAgentAskAnswer(f: JSONObject) {
        val p = pendingAsk ?: return
        if (f.optString("ask_id") != p.askId) return
        val choices = f.optJSONArray("choices")?.let { a -> (0 until a.length()).map { a.optInt(it) } } ?: emptyList()
        val labels = choices.mapNotNull { idx -> if (idx in p.choices.indices) p.choices[idx] else null }.joinToString(", ")
        val text = f.optString("text")
        askAnswerNote = listOf(labels, text).filter { it.isNotEmpty() }.joinToString(" · ").ifEmpty { "(no selection)" }
        pendingAsk = null
        buildAskCard()
    }

    private fun buildAskCard() {
        askContainer.removeAllViews()
        askChoiceBtns.clear()
        askEdit = null
        val ask = pendingAsk
        if (ask == null) {
            askContainer.visibility = if (askAnswerNote != null) View.VISIBLE else View.GONE
            if (askAnswerNote != null) {
                askContainer.addView(TextView(this).apply {
                    text = "answered: $askAnswerNote"
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                    setTextColor(0xFF4ade80.toInt())
                })
            }
            return
        }
        askContainer.visibility = View.VISIBLE
        val card = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(10), dp(8), dp(10), dp(8))
            setBackgroundColor(0xFF16161c.toInt())
        }
        card.addView(TextView(this).apply {
            text = "AGENT QUESTION"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF4ade80.toInt())
            setTypeface(Typeface.DEFAULT_BOLD)
        })
        card.addView(TextView(this).apply {
            text = ask.question
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFF3F4F6.toInt())
            setTypeface(Typeface.DEFAULT_BOLD)
            setPadding(0, dp(4), 0, dp(4))
        })
        for ((idx, choice) in ask.choices.withIndex()) {
            val b = Button(this).apply {
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setPadding(dp(8), dp(6), dp(8), dp(6))
                setOnClickListener { toggleAskChoice(idx) }
            }
            updateAskChoiceBtn(idx)
            askChoiceBtns.add(b)
            card.addView(b)
        }
        val edit = if (ask.freeText) {
            EditText(this).apply {
                hint = "or type your own answer…"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            }
        } else null
        askEdit = edit
        if (edit != null) card.addView(edit)
        val send = Button(this).apply {
            text = "Send"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setPadding(dp(16), dp(8), dp(16), dp(8))
            gravity = Gravity.END
            setOnClickListener {
                val text = edit?.text.toString().trim() ?: ""
                val choiceList = askSelected.toList().sorted()
                relay?.send(Term.agentAskAnswer(ask.askId, choiceList, text))
                pendingAsk = null
                val labels = choiceList.mapNotNull { i -> if (i in ask.choices.indices) ask.choices[i] else null }.joinToString(", ")
                askAnswerNote = listOf(labels, text).filter { it.isNotEmpty() }.joinToString(" · ").ifEmpty { "(no selection)" }
                buildAskCard()
            }
        }
        card.addView(send)
        askContainer.addView(card)
    }

    private fun toggleAskChoice(idx: Int) {
        val ask = pendingAsk ?: return
        if (ask.multi) {
            if (askSelected.contains(idx)) askSelected.remove(idx) else askSelected.add(idx)
        } else {
            askSelected.clear()
            askSelected.add(idx)
        }
        for (i in askChoiceBtns.indices) updateAskChoiceBtn(i)
    }

    private fun updateAskChoiceBtn(idx: Int) {
        val ask = pendingAsk ?: return
        askChoiceBtns.getOrNull(idx)?.let {
            val sel = askSelected.contains(idx)
            it.text = formatChoice(idx, ask.choices[idx], sel, ask.suggested == idx, ask.multi)
            it.setTextColor(if (sel) 0xFF4ade80.toInt() else 0xFF9ca3af.toInt())
        }
    }

    private fun formatChoice(idx: Int, choice: String, sel: Boolean, suggested: Boolean, multi: Boolean): String {
        val mark = if (multi) (if (sel) "◉" else "○") else (if (sel) "●" else "○")
        val sug = if (suggested) "  (suggested)" else ""
        return "$mark ${idx + 1}. $choice$sug"
    }

    // ---- Model picker ----
    private fun updateModelBar() {
        val p = panes[activePane] ?: return
        if (p.kind != "forge-chat") { modelBar.visibility = View.GONE; return }
        modelBar.visibility = View.VISIBLE
        modelChip.text = "◈ ${p.model.ifEmpty { "pick a model…" }}"
        contextLabel.text = when {
            compacting && compactingPane == activePane -> "compacting…"
            p.context.isEmpty() -> "tap to compact"
            else -> "${p.context} · tap to compact"
        }
        modelChip.setOnClickListener { openModelPicker() }
        contextLabel.setOnClickListener {
            if (compacting && compactingPane == activePane) return@setOnClickListener
            android.app.AlertDialog.Builder(this)
                .setTitle("Compact context?")
                .setMessage(
                    "Compaction summarizes the conversation so far and drops older turns.\n\n" +
                    "You can keep typing: messages sent while compacting are queued " +
                    "and sent automatically when it finishes."
                )
                .setPositiveButton("Compact") { _, _ -> startCompact() }
                .setNegativeButton("Cancel", null)
                .show()
        }
    }

    private fun startCompact() {
        if (compacting || activePane.isEmpty()) return
        val frame = Term.chatCompact(sessionId, activePane)
        compactReqId = frame.optString("req_id")
        compacting = true
        compactingPane = activePane
        compactingSince = System.currentTimeMillis()
        persistCompaction()
        handler.removeCallbacks(compactTimeoutRun)
        handler.postDelayed(compactTimeoutRun, 600_000)
        renderQueue()
        updateModelBar()
    }

    /** Compaction finished (success, failure, or timeout) — flush the queue. */
    private fun finishCompact(note: String? = null) {
        if (!compacting && queued.isEmpty()) return
        compacting = false
        compactingPane = ""
        compactingSince = 0
        compactReqId = null
        handler.removeCallbacks(compactTimeoutRun)
        clearPersistedCompaction()
        if (queued.isNotEmpty()) {
            for (q in queued) relay?.chatSend(sessionId, q.pane, q.text)
            queued.clear()
            queuedNote = note ?: "✓ queued messages sent"
            handler.postDelayed({ queuedNote = null; renderQueue() }, 4000)
        } else if (note != null) {
            queuedNote = note
            handler.postDelayed({ queuedNote = null; renderQueue() }, 4000)
        }
        renderQueue()
        updateModelBar()
    }

    /** Compaction banner + queued-message rows, pinned above the composer. */
    private fun renderQueue() {
        if (!::queueContainer.isInitialized) return
        queueContainer.removeAllViews()
        val show = (compacting && compactingPane == activePane)
            || queued.isNotEmpty() || queuedNote != null
        queueContainer.visibility = if (show) View.VISIBLE else View.GONE
        if (!show) return
        if (compacting) {
            queueContainer.addView(TextView(this).apply {
                text = "🗜 compacting context — new messages will be queued"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFFFBBF24.toInt())
                setPadding(dp(4), dp(2), dp(4), dp(2))
            })
        }
        for ((i, q) in queued.withIndex()) {
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(4), dp(2), dp(4), dp(2))
            }
            row.addView(TextView(this).apply {
                text = "⏳ queued: ${q.text.take(80)}"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF9AA0A6.toInt())
            }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            row.addView(Button(this).apply {
                text = "✕"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                minWidth = 0
                setPadding(dp(10), 0, dp(10), 0)
                setOnClickListener { queued.removeAt(i); persistCompaction(); renderQueue() }
            })
            queueContainer.addView(row, LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))
        }
        queuedNote?.let {
            queueContainer.addView(TextView(this).apply {
                text = it
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF4ADE80.toInt())
                setPadding(dp(4), dp(2), dp(4), dp(2))
            })
        }
    }

    private fun openModelPicker() {
        val pane = panes[activePane] ?: return
        val known = modelOpts[activePane]
        if (known != null) { showModelPicker(known); return }
        // request model list
        relay?.send(Term.modelList(activePane))
        modelChip.text = "loading models…"
    }

    private fun onModelListOk(f: JSONObject) {
        val reqId = f.optString("req_id")
        if (!reqId.startsWith("ml-")) return
        val paneId = f.optString("pane")
        val models = f.optJSONArray("models")?.let { a ->
            (0 until a.length()).map { Term.parseModelChoice(a.getJSONObject(it)) }
        } ?: emptyList()
        modelOpts[paneId] = models
        showModelPicker(models)
    }

    private fun showModelPicker(models: List<Term.ModelChoice>) {
        modelPw?.dismiss()
        val list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(8))
            setBackgroundColor(0xFF1a1b23.toInt())
            isVerticalScrollBarEnabled = true
        }
        for (m in models) {
            list.addView(Button(this).apply {
                text = "${m.name}  (${m.provider})"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setPadding(dp(8), dp(6), dp(8), dp(6))
                setOnClickListener {
                    relay?.send(Term.modelSet(sessionId, activePane, m.provider, m.id))
                    modelChip.text = "◈ ${m.name}"
                    modelPw?.dismiss()
                }
            })
        }
        val popup = PopupWindow(list, dp(280), dp(320), true)
        modelPw = popup
        popup.showAsDropDown(modelChip)
    }

    // ---- Chat scrollback ----
    private fun onChatHistoryOk(f: JSONObject) {
        if (f.optString("req_id") != chatHistReqId) return
        chatHistReqId = null
        val paneId = f.optString("pane")
        val pane = panes[paneId] ?: return
        val msgs = f.optJSONArray("msgs")?.let { a ->
            (0 until a.length()).map { Term.parseChatMsg(a.getJSONObject(it)) }
        } ?: return
        val existing = pane.chat.map { it.seq }.toSet()
        val add = msgs.filter { it.seq !in existing }
        pane.chat = (add + pane.chat).toMutableList()
        pane.chatHasMore = f.optBoolean("has_more", false)
        if (paneId == activePane && uiKind == "forge-chat") renderChat(pane, true)   // prepended → rebuild
    }

    private fun loadOlderChat() {
        val pane = panes[activePane] ?: return
        if (!pane.chatHasMore || chatHistReqId != null) return
        val before = pane.chat.firstOrNull()?.seq ?: return
        chatHistReqId = "ch-pending"
        val frame = Term.chatHistory(sessionId, activePane, 50, before)
        chatHistReqId = frame.optString("req_id")
        relay?.send(frame)
    }

    // ---- PTY scrollback ----
    private fun onScrollback(f: JSONObject) {
        val paneId = f.optString("pane")
        if (paneId != activePane || uiKind != "pty") return
        val lines = f.optJSONArray("lines")?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: return
        showScrollback(lines)
    }

    private fun showScrollback(lines: List<String>) {
        val text = lines.joinToString("\n")
        val tv = TextView(this).apply {
            this.text = text
            setTypeface(Typeface.MONOSPACE)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFFd1d5db.toInt())
            setPadding(dp(8), dp(8), dp(8), dp(8))
            setBackgroundColor(0xFF101418.toInt())
        }
        val pw = PopupWindow(ScrollView(this).apply { addView(tv) },
            LinearLayout.LayoutParams.MATCH_PARENT, dp(400), true)
        pw.showAtLocation(content, Gravity.TOP or Gravity.CENTER_HORIZONTAL, 0, 0)
    }

    // ---- rendering ----
    private fun rebuildTabs() {
        paneTabs.removeAllViews()
        if (panes.size <= 1) { paneTabs.visibility = View.GONE; return }
        paneTabs.visibility = View.VISIBLE
        for ((id, p) in panes) {
            val b = Button(this).apply {
                text = if (id == activePane) "▣ ${shortLabel(id, p)}" else shortLabel(id, p)
                setTextColor(if (id == activePane) 0xFF7fd4ff.toInt() else 0xFF9aa0a6.toInt())
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setPadding(dp(8), dp(2), dp(8), dp(2))
                setOnClickListener { activePane = id; sendPaneSelect(id); renderActive() }
            }
            paneTabs.addView(b)
        }
    }

    private fun shortLabel(id: String, p: Pane): String =
        (if (p.kind == "forge-chat") "agent" else "term") + "·" + id.take(4)

    private fun renderActive() {
        val p = panes[activePane] ?: return
        uiKind = p.kind
        content.removeAllViews()
        if (p.kind == "forge-chat") {
            buildChatViews()
            renderChat(p, true)
        } else {
            buildTerminalView()
        }
        buildInputArea()
        updateModelBar()
        renderQueue()
        if (uiKind == "pty") handler.post { focusHidden() }
    }

    private fun buildTerminalView() {
        val tv = TerminalView(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f)
            onGeometry = { c, r -> if (uiKind == "pty" && c > 0 && r > 0) sendResize() }
            isClickable = true
            setOnClickListener { focusHidden() }
        }
        term = tv
        content.addView(tv)
        val p = panes[activePane]
        if (p != null) tv.setScreen(p.cols, p.rows, p.lines, p.cursor)
        tv.focused = true
    }

    private fun buildChatViews() {
        chatScroll = ScrollView(this)
        chatBox = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(10), dp(8), dp(10), dp(8))
        }
        chatScroll.addView(chatBox)
        content.addView(chatScroll, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
        // detect scroll-to-top for loading older
        chatScroll.setOnScrollChangeListener(object : android.view.View.OnScrollChangeListener {
            override fun onScrollChange(v: android.view.View, left: Int, top: Int, oldLeft: Int, oldTop: Int) {
                if (top <= 50 && uiKind == "forge-chat") loadOlderChat()
            }
        })
    }

    private fun renderChat(p: Pane, full: Boolean) {
        if (full) { chatBox.removeAllViews(); renderedSeq = 0 }
        for (m in p.chat) {
            if (m.seq <= renderedSeq) continue   // already on screen
            chatBox.addView(renderChatMsg(m))
            renderedSeq = m.seq
        }
        chatScrollBottom()
    }

    /// Scroll to the newest message WITHOUT touching focus —
    /// ScrollView.fullScroll() runs a focus search that hands focus to
    /// the selectable bubbles, yanking it from the composer mid-typing
    /// (and collapsing the keyboard).
    private fun chatScrollBottom() {
        chatScroll.post {
            val content = chatScroll.getChildAt(0) ?: return@post
            chatScroll.scrollTo(0, (content.height - chatScroll.height).coerceAtLeast(0))
        }
    }

    private fun chatStickBottom() {
        val nearBottom = chatScroll.scrollY + chatScroll.height >= chatBox.height - dp(48)
        if (nearBottom) chatScrollBottom()
    }

    private fun copyToClipboard(text: String): Boolean {
        if (text.isBlank()) return false
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as android.content.ClipboardManager
        cm.setPrimaryClip(android.content.ClipData.newPlainText("ranch", text))
        return true
    }

    private fun prettyJson(s: String): String = try {
        val t = s.trimStart()
        when {
            t.startsWith("{") -> org.json.JSONObject(s).toString(2)
            t.startsWith("[") -> org.json.JSONArray(s).toString(2)
            else -> s
        }
    } catch (e: Exception) {
        s
    }

    /// The one argument that explains the call: the command for shell
    /// tools, the path for file tools, the pattern for search tools.
    private fun toolKeyArg(name: String?, argsJson: String?): String? {
        val a = argsJson?.let {
            try { org.json.JSONObject(it) } catch (e: Exception) { null }
        } ?: return null
        val keys = when (name?.lowercase()) {
            "bash", "sh", "exec", "shell", "run" ->
                listOf("command", "cmd", "script")
            "read", "write", "edit" ->
                listOf("path", "file_path", "file")
            "grep", "search", "find", "rg" ->
                listOf("pattern", "query")
            else ->
                listOf("command", "cmd", "path", "file_path", "pattern", "query", "url")
        }
        for (k in keys) {
            val v = a.opt(k)
            if (v is String && v.isNotBlank()) return v.replace("\n", " ⏎ ")
        }
        return null
    }

    /// A labelled dark mono box (command / output) inside an expanded
    /// tool row.
    private fun toolBlock(label: String, body: String, labelColor: Int = 0xFF5C636B.toInt()): android.view.View {
        val box = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = android.graphics.drawable.GradientDrawable().apply {
                cornerRadius = dp(8).toFloat()
                setColor(0xFF14181D.toInt())
            }
            setPadding(dp(8), dp(6), dp(8), dp(6))
        }
        box.addView(TextView(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 9f)
            setTextColor(labelColor)
        })
        box.addView(TextView(this).apply {
            text = body
            setTextIsSelectable(true)
            setTypeface(Typeface.MONOSPACE)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFFC9D1D9.toInt())
        })
        return box
    }

    /// Tool results arrive as pi's envelope
    /// `{content:[{type:"text",text:…}], isError:…}` — unwrap the text
    /// for display; fall back to the pretty-printed raw JSON.
    private fun toolOutputDisplay(raw: String): Pair<String, String> {
        try {
            val o = org.json.JSONObject(raw)
            val content = o.opt("content")
            if (content is org.json.JSONArray) {
                val sb = StringBuilder()
                for (i in 0 until content.length()) {
                    val item = content.optJSONObject(i) ?: continue
                    val t = item.optString("text")
                    if (t.isNotEmpty()) {
                        if (sb.isNotEmpty()) sb.append('\n')
                        sb.append(t)
                    }
                }
                if (sb.isNotEmpty()) {
                    val label = if (o.optBoolean("isError", false)) "error" else "output"
                    return label to sb.toString()
                }
            }
        } catch (e: Exception) {
        }
        return "output" to raw
    }

    private fun renderChatMsg(m: Term.ChatMsg): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(4), 0, dp(4))
        }
        if (m.toolName != null) {
            // tool row: collapsed = one line with the command so the call
            // is readable at a glance; tap expands labelled command +
            // output blocks; long-press copies both
            val dur = m.durationMs?.let {
                if (it >= 1000) " · " + String.format(java.util.Locale.US, "%.1fs", it / 1000.0)
                else " · ${it}ms"
            } ?: ""
            val outRaw = (m.toolOutput ?: m.text).takeIf { it.isNotEmpty() }
            val outDisp = outRaw?.let { toolOutputDisplay(prettyJson(it)) }
            val cmdBody = m.toolArgs?.let { prettyJson(it) }
            val summary = toolKeyArg(m.toolName, m.toolArgs)
                ?.let { if (it.length > 70) it.substring(0, 70) + " …" else it }
            val baseTitle = buildString {
                append("⚙ "); append(m.toolName)
                if (summary != null) { append(" · "); append(summary) }
                append(dur)
            }
            var open = false
            val details = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(10), dp(2), dp(10), dp(4))
                visibility = android.view.View.GONE
            }
            if (cmdBody != null) details.addView(toolBlock("command", cmdBody))
            if (outDisp != null) {
                details.addView(View(this), LinearLayout.LayoutParams(1, dp(4)))
                details.addView(
                    toolBlock(
                        outDisp.first,
                        if (outDisp.second.length > 8000) outDisp.second.substring(0, 8000) + " …" else outDisp.second,
                        if (outDisp.first == "error") 0xFFE06C75.toInt() else 0xFF5C636B.toInt()
                    )
                )
            }
            val expandable = cmdBody != null || outDisp != null
            val title = TextView(this).apply {
                text = if (expandable) "$baseTitle ▼" else baseTitle
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF8bd08b.toInt())
                setPadding(dp(10), dp(6), dp(10), if (expandable) dp(2) else dp(6))
                if (expandable) {
                    setOnClickListener {
                        open = !open
                        text = "$baseTitle ${if (open) "▲" else "▼"}"
                        details.visibility = if (open) android.view.View.VISIBLE else android.view.View.GONE
                    }
                    setOnLongClickListener {
                        val all = listOfNotNull(cmdBody, outDisp?.second).joinToString("\n\n")
                        if (copyToClipboard(all)) {
                            text = "$baseTitle · copied ✓"
                            handler.postDelayed({
                                text = "$baseTitle ${if (open) "▲" else "▼"}"
                            }, 1500)
                        }
                        true
                    }
                }
            }
            wrap.addView(title)
            wrap.addView(details)
            return wrap
        }
        // chat bubble: user right (blue), agent left (dark) — like every
        // other messaging app, so scanning the conversation is effortless.
        // Text is selectable: long-press → selection handles → copy part
        // or all (system toolbar incl. Select All)
        val isUser = m.role == "user"
        val body = if (m.text.length > 8000) m.text.substring(0, 8000) + " …" else m.text
        val bubble = TextView(this).apply {
            text = markdownToSpannable(body)
            setTextIsSelectable(true)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(if (isUser) 0xFFEAF2FF.toInt() else 0xFFE5E5E5.toInt())
            setPadding(dp(12), dp(8), dp(12), dp(8))
            background = android.graphics.drawable.GradientDrawable().apply {
                cornerRadius = dp(14).toFloat()
                setColor(if (isUser) 0xFF24557E.toInt() else 0xFF1C2127.toInt())
            }
        }
        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        if (isUser) {
            row.addView(View(this), LinearLayout.LayoutParams(0, 1, 0.22f))
            row.addView(bubble, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 0.78f))
        } else {
            row.addView(bubble, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 0.92f))
            row.addView(View(this), LinearLayout.LayoutParams(0, 1, 0.08f))
        }
        wrap.addView(row)
        val meta = listOf(if (isUser) "you" else "agent", fmtTime(m.createdAt))
            .filter { it.isNotEmpty() }.joinToString(" · ")
        if (meta.isNotEmpty()) {
            wrap.addView(TextView(this).apply {
                text = "$meta · long-press to select"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
                setTextColor(0xFF5C636B.toInt())
                setPadding(dp(6), dp(2), dp(6), 0)
                gravity = if (isUser) Gravity.END else Gravity.START
            })
        }
        return wrap
    }

    private fun fmtTime(iso: String?): String {
        if (iso.isNullOrEmpty()) return ""
        return try {
            java.time.format.DateTimeFormatter.ofPattern("HH:mm")
                .withZone(java.time.ZoneId.systemDefault())
                .format(java.time.Instant.parse(iso))
        } catch (_: Exception) { "" }
    }

    // basic markdown: **bold**, *italic*, `code`, # headers, - lists.
    // Hand-rolled inline scanner — the old nested-quantifier regex
    // (`(.+?)*`) hit catastrophic backtracking on long agent messages and
    // ANR'd the chat. This is O(n), no backtracking possible.
    private fun markdownToSpannable(raw: String): SpannableStringBuilder {
        val sb = SpannableStringBuilder()
        val lines = raw.replace("\r\n", "\n").split("\n")
        for ((li, line) in lines.withIndex()) {
            if (li > 0) sb.append("\n")
            val trimmed = line.trimStart()
            val isHeader = trimmed.startsWith("#")
            val isList = trimmed.startsWith("- ") || trimmed.startsWith("* ")
            var content = trimmed
            if (isHeader) content = content.replace(Regex("^#+\\s*"), "")
            if (isList) content = "• " + content.replace(Regex("^[-*]\\s*"), "")
            val pos = sb.length
            mdInline(sb, content)
            if (isHeader && sb.length > pos) {
                sb.setSpan(StyleSpan(Typeface.BOLD), pos, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                sb.setSpan(ForegroundColorSpan(0xFF7fd4ff.toInt()), pos, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
        }
        return sb
    }

    /** Inline **bold** / *italic* / `code` — linear scan, no regex. */
    private fun mdInline(sb: SpannableStringBuilder, text: String) {
        var i = 0
        val n = text.length
        while (i < n) {
            val c = text[i]
            val markLen = when {
                c == '*' && i + 1 < n && text[i + 1] == '*' -> 2
                c == '*' || c == '`' -> 1
                else -> 0
            }
            if (markLen == 0) { sb.append(c); i++; continue }
            val closeSeq = if (c == '`') "`" else if (markLen == 2) "**" else "*"
            val close = text.indexOf(closeSeq, i + markLen)
            if (close < 0) { sb.append(c); i++; continue }
            val s = sb.length
            sb.append(text.substring(i + markLen, close))
            when {
                c == '`' -> sb.setSpan(ForegroundColorSpan(0xFF4ade80.toInt()), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                markLen == 2 -> sb.setSpan(StyleSpan(Typeface.BOLD), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                else -> sb.setSpan(StyleSpan(Typeface.ITALIC), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
            i = close + markLen
        }
    }

    // ---- input area ----
    private fun buildInputArea() {
        inputArea.removeAllViews()
        if (uiKind == "forge-chat") buildChatInput() else buildPtyInput()
    }

    private fun buildPtyInput() {
        hiddenEdit = EditText(this).apply {
            alpha = 0f
            setBackgroundColor(0)
            setTextSize(TypedValue.COMPLEX_UNIT_PX, 1f)
            setPadding(0, 0, 0, 0)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN
            setText(" ")
            addTextChangedListener(object : android.text.TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
                override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
                override fun afterTextChanged(e: android.text.Editable?) {
                    val t = e?.toString() ?: ""
                    if (t.isEmpty()) {
                        sendPty("\u007F")
                        hiddenEdit.setText(" ")
                    } else if (t != " ") {
                        val added = if (t.startsWith(" ")) t.substring(1) else t
                        if (added.isNotEmpty()) {
                            sendPty(added.replace("\n", "\r"))
                            // predictive echo at the last-known cursor
                            val cur = panes[activePane]?.cursor
                            if (cur != null) term?.setPrediction(cur.first, cur.second, added)
                        }
                        hiddenEdit.setText(" ")
                    }
                }
            })
        }
        inputArea.addView(hiddenEdit, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 1))

        val keys = HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        val keyRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for (k in listOf("←", "↑", "↓", "→", "Enter", "Esc", "Tab", "Ctrl-C", "Ctrl-D", "Ctrl-L", "hist")) {
            keyRow.addView(keyButton(k) {
                if (k == "hist") {
                    relay?.send(Term.scrollbackReq(sessionId, activePane))
                } else {
                    val seq = Term.KEY_SEQ[k] ?: ""
                    sendPty(seq)
                }
                focusHidden()
            })
        }
        keys.addView(keyRow)
        inputArea.addView(keys, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))
    }

    private fun keyButton(label: String, onClick: () -> Unit): Button =
        Button(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(dp(14), dp(8), dp(14), dp(8))
            minWidth = dp(56)
            setOnClickListener { onClick() }
        }

    private fun buildChatInput() {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(10), dp(6), dp(10), dp(8))
        }
        chatEdit = EditText(this).apply {
            hint = "message the agent…"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            maxLines = 4
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(0xFFE5E5E5.toInt())
            setHintTextColor(0xFF5C636B.toInt())
            val pad = dp(14)
            setPadding(pad, dp(10), pad, dp(10))
            background = android.graphics.drawable.GradientDrawable().apply {
                cornerRadius = dp(24).toFloat()
                setColor(0xFF1C2127.toInt())
            }
            imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI
            setOnEditorActionListener { _, _, _ -> sendChat(); true }
        }
        // stop button: only visible while this pane's agent has a turn in
        // flight. Interrupts the running work immediately but keeps the
        // session + conversation (pi `abort` RPC / forge /interrupt).
        stopBtn = Button(this).apply {
            text = "⏹ stop"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFE06C75.toInt())
            setPadding(dp(10), dp(4), dp(10), dp(4))
            minWidth = dp(56)
            visibility = View.GONE
            setOnClickListener { sendInterrupt() }
        }
        val send = Button(this).apply {
            text = "➤"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            minWidth = dp(60)
            setPadding(0, dp(6), 0, dp(6))
        }
        send.setOnClickListener { sendChat() }
        row.addView(chatEdit, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        row.addView(stopBtn)
        row.addView(send)
        // explicit params: bare addView on a horizontal LinearLayout defaults
        // to WRAP_CONTENT, which squeezed the composer to its content width
        inputArea.addView(row, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))
        updateStopButton()
    }

    /// Show the ⏹ stop button iff the active pane is an agent pane with a
    /// turn in flight. Called from input-area builds, pane switches, chat
    /// frames (user row landing), and `meta { kind: "agent" }` updates.
    private fun updateStopButton() {
        val b = stopBtn ?: return
        val working = panes[activePane]?.agentWorking == true && uiKind == "forge-chat"
        b.visibility = if (working) View.VISIBLE else View.GONE
    }

    private fun sendInterrupt() {
        if (activePane.isEmpty()) return
        relay?.send(Term.interrupt(sessionId, activePane))
        // the idle meta + the "⏹ interrupted" system row follow; hide the
        // button optimistically so a double-tap can't re-fire the RPC
        panes[activePane]?.agentWorking = false
        updateStopButton()
    }

    private fun focusHidden() {
        if (uiKind != "pty") return
        hiddenEdit.requestFocus()
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.showSoftInput(hiddenEdit, InputMethodManager.SHOW_IMPLICIT)
    }

    // ---- send helpers ----
    private fun sendPty(text: String) {
        if (text.isEmpty() || activePane.isEmpty()) return
        relay?.input(sessionId, activePane, text)
    }
    private fun sendChat() {
        val t = chatEdit.text.toString().trim()
        if (t.isEmpty()) return
        // mid-compaction: hold the message and send it when the queue flushes
        if (compacting && activePane == compactingPane) {
            queued.add(Queued(activePane, t))
            chatEdit.setText("")
            persistCompaction()
            renderQueue()
            return
        }
        relay?.chatSend(sessionId, activePane, t)
        chatEdit.setText("")
    }
    private fun sendResize() {
        val tv = term ?: return
        relay?.resize(sessionId, tv.cols, tv.rows)
    }
    private fun sendPaneSelect(pane: String) {
        relay?.send(JSONObject().put("t", "PaneSelect").put("id", Term.newId())
            .put("client", Term.CLIENT).put("session", sessionId).put("pane", pane))
    }

    private fun renameSession() {
        val input = EditText(this).apply { setText(sessionName.ifEmpty { sessionId.take(8) }) }
        android.app.AlertDialog.Builder(this)
            .setTitle("rename session")
            .setView(input)
            .setPositiveButton("rename") { _, _ ->
                val n = input.text.toString().trim()
                if (n.isNotEmpty()) {
                    relay?.send(Term.sessionsRename(sessionId, n))
                    titleView.text = n
                    sessionName = n
                }
            }
            .setNegativeButton("cancel", null)
            .show()
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@SessionActivity).apply { text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f) })
            addView(Button(this@SessionActivity).apply {
                text = "Back"; setOnClickListener { finish() }
            })
        }
}
