package dev.ranch.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.text.TextWatcher
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/**
 * The session screen (Phase 2): attach to one session and render its active
 * pane — a terminal grid for PTY panes, a chat list for agent (forge-chat)
 * panes. Input is captured per pane kind. Port of mobile/screens/Terminal.tsx.
 */
class SessionActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null

    // ---- pane state ----
    private data class Pane(
        var kind: String = "pty",
        var cols: Int = 80,
        var rows: Int = 24,
        var lines: List<String> = emptyList(),
        var cursor: Triple<Int, Int, Boolean>? = null,
        var chat: MutableList<Term.ChatMsg> = mutableListOf(),
        var model: String = "",
        var seq: Long = 0,
    )
    private val panes = LinkedHashMap<String, Pane>()
    private var activePane = ""
    private var sessionName = ""
    private var sessionId = ""
    private val lastSeq = LinkedHashMap<String, Long>()
    private var uiKind = ""          // kind of the pane currently rendered
    private var agentStatus = ""     // "working" | "idle" | ""

    // ---- views ----
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

    private lateinit var sink: (JSONObject) -> Unit

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
        val session = sessionId

        // ---------- layout ----------
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
            setPadding(dp(8), 0, 0, 0); text = sessionName.ifEmpty { session.take(8) }
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

        // content (terminal OR chat)
        content = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(content, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // input area (rebuilt per pane kind)
        inputArea = LinearLayout(this)
        root.addView(inputArea, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))

        setContentView(root)

        // ---------- subscribe + attach ----------
        sink = { frame -> onFrame(frame, session) }
        r.addSink(sink)
        r.attach(session)
    }

    override fun onResume() {
        super.onResume()
        term?.focused = true
    }
    override fun onPause() {
        term?.focused = false
        super.onPause()
    }
    override fun onDestroy() {
        relay?.let {
            it.removeSink(sink)
            it.detach()
        }
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    // ================= frame handling (WS thread -> main) =================
    private fun onFrame(f: JSONObject, session: String) {
        val sid = f.optString("session")
        if (sid != session) return
        handler.post { handle(f, session) }
    }

    private fun handle(f: JSONObject, session: String) {
        when (f.optString("t")) {
            "Snapshot" -> onSnapshot(f)
            "Update" -> onUpdate(f, session)
            "Chat" -> onChat(f)
            "Meta" -> onMeta(f)
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
                lines = lines,
                cursor = cursor,
                chat = chat,
                model = p.optString("model"),
                seq = p.optLong("seq", 0),
            )
            if (p.optLong("seq", 0) > 0) lastSeq[id] = p.optLong("seq")
        }
        panes.clear(); panes.putAll(next)
        rebuildTabs()
        renderActive()
        // request the phone's geometry for PTY panes
        if (uiKind == "pty") sendResize()
    }

    private fun onUpdate(f: JSONObject, session: String) {
        val paneId = f.optString("pane")
        val pane = panes[paneId] ?: return
        val seq = f.optLong("seq", 0)
        val prev = lastSeq[paneId] ?: 0
        if (prev > 0 && seq != prev + 1L) {
            if (seq <= prev) return            // duplicate / reorder -> drop
            // forward gap -> re-attach so the daemon re-snapshots
            lastSeq.remove(paneId)
            relay?.attach(session, null)
            return
        }
        lastSeq[paneId] = seq
        val cols = f.optInt("cols", pane.cols)
        val rows = f.optInt("rows", pane.rows)
        pane.cols = cols; pane.rows = rows
        val newLines = ArrayList<String>(pane.lines)
        val upd = f.optJSONArray("rows_upd")
        if (upd != null) {
            for (i in 0 until upd.length()) {
                val pair = upd.getJSONArray(i)
                val y = pair.optInt(0)
                val text = pair.optString(1)
                while (newLines.size <= y) newLines.add("")
                newLines[y] = text
            }
        }
        pane.lines = newLines
        val c = f.optJSONObject("cursor")?.let {
            Triple(it.optInt("x"), it.optInt("y"), it.optBoolean("visible", true))
        }
        if (c != null) pane.cursor = c
        if (paneId == activePane && uiKind == "pty") term?.applyUpdate(
            upd?.let { a -> (0 until a.length()).map { i ->
                val p = a.getJSONArray(i); p.optInt(0) to p.optString(1)
            } } ?: emptyList(), c)
    }

    private fun onChat(f: JSONObject) {
        val paneId = f.optString("pane")
        val pane = panes[paneId] ?: return
        val msgs = f.optJSONArray("msgs")?.let { a ->
            (0 until a.length()).map { Term.parseChatMsg(a.getJSONObject(it)) }
        } ?: return
        if (f.optBoolean("reset")) pane.chat = msgs.toMutableList()
        else {
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
            "exited" -> {
                statusView.text = "session ended"
                handler.postDelayed({ finish() }, 1500)
            }
            "model" -> {
                val p = f.optString("pane")
                panes[p]?.let { pane ->
                    if (pane.kind == "forge-chat") {
                        pane.model = f.optString("status")
                        if (p == activePane && uiKind == "forge-chat") renderChat(pane)
                    }
                }
            }
        }
    }

    // ================= rendering =================

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
        chatBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(dp(10), dp(8), dp(10), dp(8)) }
        chatScroll.addView(chatBox)
        content.addView(chatScroll, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
    }

    private fun renderChat(p: Pane) {
        val modelLine = p.model.takeIf { it.isNotEmpty() }
        chatBox.removeAllViews()
        if (modelLine != null) chatBox.addView(chatHeaderLine("model: $modelLine"))
        for (m in p.chat) {
            val isTool = !m.toolName.isNullOrEmpty()
            val body = if (isTool) toolLine(m) else messageLine(m)
            chatBox.addView(body)
        }
        // scroll to bottom
        chatScroll.post { chatScroll.fullScroll(View.FOCUS_DOWN) }
    }

    private fun messageLine(m: Term.ChatMsg): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, dp(6), 0, dp(6))
        }
        val label = TextView(this).apply {
            text = when (m.role) {
                "user" -> "you"
                "assistant" -> "agent"
                else -> m.role
            }
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(if (m.role == "user") 0xFF7fd4ff.toInt() else 0xFF9aa0a6.toInt())
        }
        val text = TextView(this).apply {
            this.text = m.text
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFE5E5E5.toInt())
        }
        wrap.addView(label)
        if (m.text.isNotEmpty()) wrap.addView(text)
        return wrap
    }

    private fun toolLine(m: Term.ChatMsg): LinearLayout {
        val wrap = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(6), dp(4), dp(6), dp(4))
            setBackgroundColor(0xFF1b2126.toInt())
        }
        val title = TextView(this).apply {
            val out = m.toolOutput?.takeIf { it.isNotEmpty() }
            val dur = m.durationMs?.let { " · ${it}ms" } ?: ""
            text = "⚙ ${m.toolName ?: "tool"}$dur"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF8bd08b.toInt())
        }
        wrap.addView(title)
        val detail = (m.toolOutput ?: m.text).takeIf { it.isNotEmpty() }
        if (detail != null) {
            wrap.addView(TextView(this).apply {
                text = detail.take(500); setTypeface(android.graphics.Typeface.MONOSPACE)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setTextColor(0xFF9aa0a6.toInt())
            })
        }
        return wrap
    }

    private fun chatHeaderLine(s: String): TextView = TextView(this).apply {
        text = s; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f); setTextColor(0xFF7f7f7f.toInt())
        setPadding(0, 0, 0, dp(4))
    }

    // ---- input area (rebuilt per pane kind) ----
    private fun buildInputArea() {
        inputArea.removeAllViews()
        inputArea.removeAllViewsInLayout()
        if (uiKind == "forge-chat") buildChatInput() else buildPtyInput()
    }

    private fun buildPtyInput() {
        // hidden sentinel-space EditText for character capture. Kept VISIBLE
        // but 1px-tall + transparent (INVISIBLE views can drop IME focus on
        // some Android versions); the sentinel-space trick makes each
        // keystroke a discrete text change.
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
                        sendPty("\u007F")           // backspace deleted the sentinel -> DEL
                        hiddenEdit.setText(" ")
                    } else if (t != " ") {
                        val added = if (t.startsWith(" ")) t.substring(1) else t
                        if (added.isNotEmpty()) sendPty(added.replace("\n", "\r"))
                        hiddenEdit.setText(" ")
                    }
                }
            })
        }
        inputArea.addView(hiddenEdit, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 1))

        val keys = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        for (k in listOf("←", "↑", "↓", "→", "Enter", "Esc", "Tab", "Ctrl-C", "Ctrl-D", "Ctrl-L")) {
            keys.addView(keyButton(k) {
                val seq = Term.KEY_SEQ[k] ?: Term.KEY_SEQ["Enter"] ?: ""
                sendPty(seq)
                focusHidden()
            })
        }
        inputArea.addView(keys)
    }

    private fun keyButton(label: String, onClick: () -> Unit): Button {
        return Button(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFFE5E5E5.toInt())
            setPadding(dp(8), dp(6), dp(8), dp(6))
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
            setOnClickListener { onClick(); focusHidden() }
        }
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
        hiddenEdit.requestFocus()
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.showSoftInput(hiddenEdit, InputMethodManager.SHOW_IMPLICIT)
    }

    // ---- send helpers ----
    private fun sendPty(text: String) {
        if (text.isEmpty() || activePane.isEmpty()) return
        relay?.input(session(), activePane, text)
    }
    private fun sendChat() {
        val t = chatEdit.text.toString().trim()
        if (t.isEmpty()) return
        relay?.chatSend(session(), activePane, t)
        chatEdit.setText("")
    }
    private fun sendResize() {
        val tv = term ?: return
        relay?.resize(session(), tv.cols, tv.rows)
    }
    private fun sendPaneSelect(pane: String) {
        relay?.send(JSONObject().put("t", "PaneSelect").put("id", Term.newId())
            .put("client", Term.CLIENT).put("session", session()).put("pane", pane))
    }
    private fun session(): String = sessionId

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
