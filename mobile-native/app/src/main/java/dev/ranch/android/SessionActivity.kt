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
    )
    private val panes = LinkedHashMap<String, Pane>()
    private var activePane = ""
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

        // input area
        inputArea = LinearLayout(this)
        root.addView(inputArea, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        setContentView(root)

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
                val rid = f.optString("req_id")
                if (rid.isNotEmpty()) statusView.text = "err: ${f.optString("message")}"
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
        panes.clear(); panes.putAll(next)
        rebuildTabs()
        renderActive()
        updateModelBar()
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
        if (paneId == activePane && uiKind == "forge-chat") renderChat(pane)
    }

    private fun onMeta(f: JSONObject) {
        when (f.optString("kind")) {
            "agent" -> {
                agentStatus = f.optString("status", "")
                statusView.text = when (agentStatus) {
                    "working" -> "● working"
                    "idle" -> "● idle"
                    else -> ""
                }
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
                }
            }
            "exited" -> {
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
        contextLabel.text = p.context
        modelChip.setOnClickListener { openModelPicker() }
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
        if (paneId == activePane && uiKind == "forge-chat") renderChat(pane)
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
            renderChat(p)
        } else {
            buildTerminalView()
        }
        buildInputArea()
        updateModelBar()
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

    private fun renderChat(p: Pane) {
        chatBox.removeAllViews()
        for (m in p.chat) {
            chatBox.addView(renderChatMsg(m))
        }
        if (p.chatHasMore) {
            chatBox.addView(TextView(this).apply {
                text = "… loading older …"
                setTextColor(0xFF6b7280.toInt())
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            })
        }
        chatScroll.post { chatScroll.fullScroll(View.FOCUS_DOWN) }
    }

    private fun renderChatMsg(m: Term.ChatMsg): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(6), 0, dp(6))
        }
        val label = TextView(this).apply {
            text = when (m.role) { "user" -> "you"; "assistant" -> "agent"; else -> m.role }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(if (m.role == "user") 0xFF7fd4ff.toInt() else 0xFF9aa0a6.toInt())
        }
        wrap.addView(label)
        if (m.toolName != null) {
            // tool call: collapsed
            val dur = m.durationMs?.let { " · ${it}ms" } ?: ""
            val title = TextView(this).apply {
                text = "⚙ ${m.toolName}$dur"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(0xFF8bd08b.toInt())
                setPadding(dp(6), dp(4), dp(6), dp(4))
                setBackgroundColor(0xFF1b2126.toInt())
            }
            wrap.addView(title)
            val detail = (m.toolOutput ?: m.text).takeIf { it.isNotEmpty() }
            if (detail != null) {
                wrap.addView(TextView(this).apply {
                    text = detail.take(500)
                    setTypeface(Typeface.MONOSPACE)
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                    setTextColor(0xFF9aa0a6.toInt())
                })
            }
        } else {
            // regular message with basic markdown
            val md = markdownToSpannable(m.text)
            wrap.addView(TextView(this).apply {
                text = md
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(0xFFE5E5E5.toInt())
            })
        }
        return wrap
    }

    // basic markdown: **bold**, `code`, # headers, - list
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
            var pos = sb.length
            // process inline **bold** and `code`
            val remaining = content
            val boldRe = Regex("\\*\\*(.+?)*\\*|\\*(.+?)*\\*|`(.+?)`")
            var last = 0
            for (m in boldRe.findAll(remaining)) {
                sb.append(remaining.substring(last, m.range.first))
                val s = sb.length
                if (m.groupValues[0].startsWith("**")) {
                    sb.append(m.groupValues[0].removePrefix("**").removeSuffix("**"))
                    sb.setSpan(StyleSpan(android.graphics.Typeface.BOLD), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                } else if (m.groupValues[0].startsWith("*")) {
                    sb.append(m.groupValues[0].removePrefix("*").removeSuffix("*"))
                    sb.setSpan(StyleSpan(android.graphics.Typeface.ITALIC), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                } else {
                    sb.append(m.groupValues[0].removePrefix("`").removeSuffix("`"))
                    sb.setSpan(ForegroundColorSpan(0xFF4ade80.toInt()), s, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
                }
                last = m.range.last + 1
            }
            sb.append(remaining.substring(last))
            if (isHeader) {
                sb.setSpan(ForegroundColorSpan(0xFF7fd4ff.toInt()), pos, sb.length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
            }
        }
        return sb
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

        val keys = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for (k in listOf("←", "↑", "↓", "→", "Enter", "Esc", "Tab", "Ctrl-C", "Ctrl-D", "Ctrl-L", "hist")) {
            keys.addView(keyButton(k) {
                if (k == "hist") {
                    relay?.send(Term.scrollbackReq(sessionId, activePane))
                } else {
                    val seq = Term.KEY_SEQ[k] ?: ""
                    sendPty(seq)
                }
                focusHidden()
            })
        }
        inputArea.addView(keys)
    }

    private fun keyButton(label: String, onClick: () -> Unit): Button =
        Button(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(dp(6), dp(6), dp(6), dp(6))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener { onClick() }
        }

    private fun buildChatInput() {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(8), dp(4), dp(8), dp(4))
        }
        chatEdit = EditText(this).apply {
            hint = "message the agent…"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            imeOptions = EditorInfo.IME_ACTION_SEND
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
            setOnEditorActionListener { _, _, _ -> sendChat(); true }
        }
        val send = Button(this).apply { text = "Send" }
        send.setOnClickListener { sendChat() }
        row.addView(chatEdit, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        row.addView(send)
        inputArea.addView(row)
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
