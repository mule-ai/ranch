package dev.ranch.android

import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import org.json.JSONObject
import java.io.IOException
import java.util.concurrent.Executors
import kotlin.concurrent.Volatile

/**
 * Phase 4+ — IDE. Browse the daemon host's filesystem, open a file in a real
 * code editor (CodeMirror 5 in a WebView → true syntax highlighting, line
 * numbers, undo, bracket matching), edit + save with mtime conflict
 * detection, download a host file to the phone (SAF save-as), and upload a
 * phone file to the host (SAF open). A toggle reveals hidden (dot) files —
 * the one gap the RN editor left, and the most common way you'll edit
 * `~/.config`, `~/.bashrc`, etc.
 *
 * Frames: `DirList`(hidden) → `DirListOk`; `FileRead` → `FileReadOk`;
 * `FileWrite`(mtime) → `FileWriteOk`; `FileDownload` → `FileDownloadOk`
 * (binary-safe, may arrive chunked); `FilePut` → `FilePutOk`; `FileChanged`
 * push (external edits → reload/keep-mine).
 */
class EditorActivity : Activity() {

    private val app get() = application as App
    private val handler = Handler(Looper.getMainLooper())
    private val exec = Executors.newSingleThreadExecutor()

    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit

    // ---- view refs ----
    private lateinit var bar: LinearLayout
    private lateinit var titleTv: TextView
    private lateinit var pathTv: TextView
    private lateinit var contentBox: LinearLayout
    private lateinit var browseBox: LinearLayout
    private lateinit var startEdit: EditText
    private lateinit var hiddenToggle: Switch
    private lateinit var web: WebView
    private lateinit var editBar: LinearLayout
    private lateinit var statusTv: TextView

    private lateinit var saveBtn: Button
    private lateinit var previewBtn: Button
    private lateinit var dirtyDot: TextView
    private lateinit var conflictReload: Button
    private lateinit var conflictKeep: Button

    // ---- browse state ----
    private var curDir: String = ""
    private var showHidden: Boolean = false

    // ---- edit state ----
    @Volatile private var previewing = false          // markdown preview showing
    @Volatile private var openFile: String? = null     // path being edited
    @Volatile private var openOriginal: String = ""    // content at load/save
    @Volatile private var draft: String = ""           // current editor content
    @Volatile private var curMtime: Long = 0

    // ---- req_id correlation ----
    @Volatile private var dirReqId: String? = null
    @Volatile private var readReqId: String? = null
    @Volatile private var writeReqId: String? = null
    @Volatile private var downloadReqId: String? = null

    // ---- pending SAF downloads (chosen save URI + host path) ----
    @Volatile private var pendingDownloadUri: Uri? = null

    // ---- WebView bridge state ----
    @Volatile private var webReady = false

    companion object {
        private const val REQ_UPLOAD = 1001
        private const val REQ_DOWNLOAD = 1002
        private const val LIST_CAP = 800 // stop rendering a pathological dir
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val r = Monitor.relay
        if (r == null) { setContentView(errorView("Start monitoring a machine first.")); return }
        relay = r
        showHidden = app.prefs.getBool("show_hidden", false)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
        }

        bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(8), dp(10), dp(8), dp(10))
            setBackgroundColor(0xFF101418.toInt())
        }
        bar.addView(Button(this).apply {
            text = "\u2039"; setTextColor(0xFF4ADE80.toInt()); setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setPadding(dp(6), 0, dp(6), 0); minWidth = 0; minimumWidth = 0
            setOnClickListener { goBack() }
        })
        titleTv = TextView(this).apply {
            text = "Files"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setTextColor(0xFFE5E5E5.toInt())
            typeface = android.graphics.Typeface.DEFAULT_BOLD
            gravity = Gravity.CENTER_VERTICAL
            ellipsize = android.text.TextUtils.TruncateAt.END; isSingleLine = true
            setPadding(dp(8), 0, dp(8), 0)
        }
        bar.addView(titleTv, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        root.addView(bar)

        // a hairline under the bar
        root.addView(View(this).apply {
            setBackgroundColor(0xFF1F2430.toInt())
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, dp(1))
        })

        pathTv = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF7FD4FF.toInt())
            typeface = android.graphics.Typeface.MONOSPACE
            setPadding(dp(12), dp(6), dp(12), dp(6))
            ellipsize = android.text.TextUtils.TruncateAt.MIDDLE
            isSingleLine = true
            setBackgroundColor(0xFF0C1015.toInt())
        }
        pathTv.setOnClickListener { copyPath() }
        root.addView(pathTv)

        contentBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(contentBox, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // --- browse box (start row + hidden toggle + list) ---
        browseBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        buildBrowseBox()
        contentBox.addView(browseBox, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // --- edit bar (save / download / wrap) ---
        editBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(8), dp(6), dp(8), dp(6))
            visibility = View.GONE
        }
        contentBox.addView(editBar)

        // --- editor WebView (CodeMirror) ---
        web = WebView(this).apply {
            setBackgroundColor(0xFF0A0A0E.toInt())
            visibility = View.GONE
            isFocusableInTouchMode = true
            // keep it fully offline: block any non-asset navigation, but let
            // the editor's local file:// loads through (false = load it).
            webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView?, url: String?): Boolean {
                    if (url?.startsWith("file:///android_asset/") == true) return false
                    Toast.makeText(context, "blocked: $url", Toast.LENGTH_SHORT).show()
                    return true
                }
            }
        }
        // the JS bridge must be registered before the page loads (its glue
        // calls window.Ranch on boot); settings + load happen in configureWebView
        web.addJavascriptInterface(JsBridge(), "Ranch")
        configureWebView(web)
        contentBox.addView(web, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        // --- bottom status / conflict row ---
        val statusRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(12), dp(6), dp(12), dp(6))
            gravity = Gravity.CENTER_VERTICAL
        }
        statusTv = TextView(this).apply {
            text = ""; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9AA0A6.toInt())
        }
        statusRow.addView(statusTv, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        conflictReload = smallBtn("reload") { clearConflict(); reloadFile() }
        conflictReload.visibility = View.GONE
        statusRow.addView(conflictReload)
        conflictKeep = smallBtn("keep") { clearConflict() }
        conflictKeep.visibility = View.GONE
        statusRow.addView(conflictKeep)
        contentBox.addView(statusRow)

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))

        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)

        buildEditButtons()
        refreshBar()
        browse(null)
    }

    override fun onResume() {
        super.onResume()
        applyFontSizes() // pick up font-size changes made in Settings
    }

    // ---- browse box ----
    private fun buildBrowseBox() {
        browseBox.removeAllViews()

        // start row: [start dir edit (flex)] [Go]
        val startRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        startEdit = EditText(this).apply {
            hint = "start dir (blank = home)"
            inputType = android.text.InputType.TYPE_CLASS_TEXT or
                android.text.InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFE5E5E5.toInt())
            setHintTextColor(0xFF6B7280.toInt())
            setBackgroundColor(0xFF1A1B23.toInt())
            setPadding(dp(10), dp(8), dp(10), dp(8))
        }
        startRow.addView(startEdit, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        startRow.addView(smallBtn("Go") { browse(startEdit.text.toString().trim().ifEmpty { null }) })
        browseBox.addView(startRow, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT
        ).apply { setMargins(dp(8), dp(8), dp(8), dp(4)) })

        // toggle row: [hidden switch (flex)] [⬆ upload]
        val optRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        hiddenToggle = Switch(this).apply {
            text = "show hidden files"
            setTextColor(0xFFC9CDD3.toInt())
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            isChecked = showHidden
            setPadding(dp(0), dp(2), dp(0), dp(2))
        }
        hiddenToggle.setOnCheckedChangeListener { _, checked ->
            showHidden = checked
            app.prefs.setBool("show_hidden", checked)
            refreshStatus(if (checked) "showing hidden entries" else "hiding hidden entries")
            browse(curDir.ifEmpty { null })
        }
        optRow.addView(hiddenToggle, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        optRow.addView(smallBtn("⬆ upload") { pickUpload() })
        browseBox.addView(optRow, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT
        ).apply { setMargins(dp(8), dp(2), dp(8), dp(4)) })

        // list
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        browseBox.addView(ScrollView(this).apply {
            addView(list)
            isVerticalScrollBarEnabled = true
        }, LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f).apply { setMargins(dp(8), dp(4), dp(8), dp(8)) })
    }

    // rebuilds the per-dir rows (idempotent: clears the list container)
    private fun renderDir(path: String, parent: String?, dirs: List<String>, files: List<String>) {
        curDir = path
        pathTv.text = path
        browseBox.removeViewAt(browseBox.childCount - 1) // drop the old list/scrollview
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        var shown = 0

        if (!parent.isNullOrEmpty()) {
            list.addView(fileRow("⌂  ..", "parent", hidden = false) { browse(parent) })
            shown++
        }
        for (d in dirs) {
            if (shown >= LIST_CAP) break
            list.addView(fileRow("📁  $d", "open", hidden = d.startsWith(".")) {
                browse("$path/$d")
            })
            shown++
        }
        for (fl in files) {
            if (shown >= LIST_CAP) break
            list.addView(fileRow(
                iconFor(fl) + "  " + fl, "",
                hidden = fl.startsWith("."),
                onDownload = { downloadFile("$path/$fl", fl) },
            ) { openFile("$path/$fl") })
            shown++
        }
        if (dirs.isEmpty() && files.isEmpty()) {
            list.addView(TextView(this).apply {
                text = if (showHidden) "(empty)" else "(empty — or enable “show hidden files”)"
                setTextColor(0xFF6B7280.toInt()); setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                setPadding(dp(6), dp(14), dp(6), dp(14))
            })
        }
        val extra = dirs.size + files.size + if (!parent.isNullOrEmpty()) 1 else 0 - shown
        if (extra > 0) {
            list.addView(TextView(this).apply {
                text = "+$extra more (list is capped at $LIST_CAP)"
                setTextColor(0xFF6B7280.toInt()); setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                setPadding(dp(6), dp(8), dp(6), dp(8))
            })
        }
        browseBox.addView(ScrollView(this).apply { addView(list); isVerticalScrollBarEnabled = true },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f).apply {
                setMargins(dp(8), dp(4), dp(8), dp(8))
            })
    }

    private fun iconFor(name: String): String = when (name.substringAfterLast('.', name).lowercase()) {
        "md", "mdx" -> "📝"
        "rs" -> "🦀"; "py" -> "🐍"; "js", "ts", "jsx", "tsx", "mjs" -> "🟨"
        "go" -> "🔵"; "java", "kt" -> "☕"; "c", "h", "cpp", "cc" -> "⚙️"
        "json", "toml", "yaml", "yml", "ini", "conf" -> "⚙️"
        "sh", "bash" -> "💻"; "html", "css" -> "🌐"; "lock" -> "🔒"
        else -> "📄"
    }

    /** A tappable row: [name (flex)] [optional download button]. */
    private fun fileRow(name: String, hint: String, hidden: Boolean,
                        onDownload: (() -> Unit)? = null, open: () -> Unit): View {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(10), dp(11), dp(10), dp(11))
            background = roundedBg(0xFF171A21.toInt(), dp(8))
            setOnClickListener { open() }
        }
        val nameTv = TextView(this).apply {
            text = name
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(if (hidden) 0xFF8A929E.toInt() else 0xFFE5E5E5.toInt())
            ellipsize = android.text.TextUtils.TruncateAt.END
            isSingleLine = true
        }
        row.addView(nameTv, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        if (hint.isNotEmpty()) {
            row.addView(TextView(this).apply {
                text = hint; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setTextColor(0xFF5B626E.toInt())
            })
        }
        if (onDownload != null) {
            row.addView(smallBtn("⬇") { onDownload() }.apply {
                setPadding(dp(8), dp(4), dp(8), dp(4))
            })
        }
        return row
    }

    // ---- edit bar buttons ----
    private fun buildEditButtons() {
        dirtyDot = TextView(this).apply {
            text = "●"; setTextColor(0xFFF59E0B.toInt()); setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            visibility = View.GONE
        }
        editBar.addView(dirtyDot)
        saveBtn = smallBtn("save") { save() }.apply {
            setTextColor(0xFFFFFFFF.toInt()); setBackgroundColor(0xFF1B6FD4.toInt())
            visibility = View.GONE
        }
        editBar.addView(saveBtn)
        previewBtn = smallBtn("preview") { togglePreview() }.apply { visibility = View.GONE }
        editBar.addView(previewBtn)
        editBar.addView(smallBtn("⬇ save to phone") {
            openFile?.let { downloadFile(it, it.substringAfterLast('/')) }
        })
    }

    private fun refreshBar() {
        val editing = openFile != null
        val modeLabel = if (editing) openFile?.substringAfterLast('/') ?: "" else "Files"
        titleTv.text = modeLabel
        titleTv.visibility = View.VISIBLE

        // browse-only controls
        browseBox.visibility = if (editing) View.GONE else View.VISIBLE
        // edit-only controls
        web.visibility = if (editing) View.VISIBLE else View.GONE
        editBar.visibility = if (editing) View.VISIBLE else View.GONE

        if (editing) {
            pathTv.text = openFile
            previewBtn.visibility = if (isMarkdown(openFile!!)) View.VISIBLE else View.GONE
            val dirty = draft != openOriginal
            saveBtn.visibility = if (dirty && !previewing) View.VISIBLE else View.GONE
            dirtyDot.visibility = if (dirty && !previewing) View.VISIBLE else View.GONE
        } else {
            dirtyDot.visibility = View.GONE
            saveBtn.visibility = View.GONE
            previewBtn.visibility = View.GONE
        }
    }

    // ---- mode switching + back ----
    private fun toBrowse() {
        openFile = null
        draft = ""
        openOriginal = ""
        previewing = false
        previewBtn.text = "preview"
        clearConflict()
        statusTv.text = ""
        refreshBar()
        browse(curDir.ifEmpty { null })
    }

    private fun goBack() {
        if (openFile != null) {
            if (draft != openOriginal) {
                confirmDiscardAndBack()
                return
            }
            toBrowse()
        } else {
            finish()
        }
    }

    private fun confirmDiscardAndBack() {
        AlertDialog.Builder(this)
            .setTitle("Unsaved changes")
            .setMessage("Discard changes to ${openFile?.substringAfterLast('/')}?")
            .setNegativeButton("Keep editing", null)
            .setPositiveButton("Discard & close") { _, _ ->
                // leave the file open on the host; just go back to the list
                toBrowse()
            }
            .show()
    }

    @Suppress("OVERRIDE_DEPRECATION")
    override fun onBackPressed() {
        goBack()
    }

    // ---- frame-driven browse / edit ----
    private fun browse(path: String?) {
        openFile = null
        refreshBar()
        val f = Term.dirList(path, showHidden)
        dirReqId = f.optString("req_id")
        refreshStatus("listing…")
        relay?.send(f)
    }

    private fun openFile(path: String) {
        pathTv.text = path
        val f = Term.fileRead(path)
        readReqId = f.optString("req_id")
        refreshStatus("reading ${path.substringAfterLast('/')}…")
        relay?.send(f)
    }

    private fun save() {
        val p = openFile ?: return
        val f = Term.fileWrite(p, draft, curMtime)
        writeReqId = f.optString("req_id")
        refreshStatus("saving…")
        relay?.send(f)
    }

    private fun reloadFile() {
        openFile?.let { p -> openFile(p) }
    }

    // ---- download a host file to the phone (SAF save-as) ----
    private fun downloadFile(path: String, name: String) {
        pendingDownloadPath = path
        val mime = Lang.guessMime(name)
        val i = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = mime
            putExtra(Intent.EXTRA_TITLE, name)
        }
        try {
            startActivityForResult(i, REQ_DOWNLOAD)
            statusTv.text = "choosing where to save $name…"
        } catch (e: Exception) {
            Toast.makeText(this, "no file manager: ${e.message}", Toast.LENGTH_LONG).show()
        }
    }

    private fun startDownloadToUri(uri: Uri) {
        val path = pendingDownloadPath ?: return
        pendingDownloadUri = uri
        val f = Term.fileDownload(path)
        downloadReqId = f.optString("req_id")
        refreshStatus("downloading ${path.substringAfterLast('/')}…")
        relay?.send(f)
    }

    @Volatile private var pendingDownloadPath: String? = null

    // ---- upload a phone file to the host (SAF open) ----
    private fun pickUpload() {
        val i = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
        }
        try {
            startActivityForResult(i, REQ_UPLOAD)
            statusTv.text = "pick a file to upload…"
        } catch (e: Exception) {
            Toast.makeText(this, "no file manager: ${e.message}", Toast.LENGTH_LONG).show()
        }
    }

    private fun startUpload(uri: Uri) {
        val name = queryName(uri)
        val mime = Lang.guessMime(name)
        pendingDownloadPath = null
        refreshStatus("uploading $name…")
        exec.execute {
            try {
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                    ?: throw IOException("could not open $name")
                if (bytes.size > 10 * 1024 * 1024) {
                    handler.post { statusTv.text = "too large to upload (max 10 MiB)" }; return@execute
                }
                val b64 = android.util.Base64.encodeToString(bytes, android.util.Base64.NO_WRAP)
                relay?.send(Term.filePut(name, b64))
            } catch (e: Exception) {
                handler.post { statusTv.text = "upload failed: ${e.message}" }
            }
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != RESULT_OK) { pendingDownloadUri = null; return }
        val uri = data?.data ?: return
        when (requestCode) {
            REQ_UPLOAD -> startUpload(uri)
            REQ_DOWNLOAD -> startDownloadToUri(uri)
        }
    }

    // ---- frame dispatch ----
    private fun onFrame(f: JSONObject) {
        val rid = f.optString("req_id")
        when (f.optString("t")) {
            "DirListOk" -> {
                if (rid != dirReqId) return
                dirReqId = null
                val parent = f.optString("parent").takeIf { it.isNotEmpty() }
                val dirs = optStrList(f, "dirs")
                val files = optStrList(f, "files")
                renderDir(f.optString("path"), parent, dirs, files)
            }
            "FileReadOk" -> {
                if (rid != readReqId) return
                readReqId = null
                val path = f.optString("path")
                openFile = path
                openOriginal = f.optString("content")
                draft = openOriginal
                curMtime = f.optLong("mtime")
                refreshStatus("${f.optLong("size")} bytes · ${Lang.langForPath(path)}")
                previewing = false
                previewBtn.text = "preview"
                refreshBar()
                pathTv.text = path
                if (webReady) pushDoc(draft, Lang.modeForPath(path))
            }
            "FileWriteOk" -> {
                if (rid != writeReqId) return
                writeReqId = null
                curMtime = f.optLong("mtime")
                openOriginal = draft // now clean
                refreshBar()
                refreshStatus("saved ✓")
                handler.postDelayed({ if (openOriginal == draft) statusTv.text = "" }, 2000)
            }
            "FileDownloadOk" -> {
                if (rid != downloadReqId) return
                downloadReqId = null
                val uri = pendingDownloadUri ?: return
                pendingDownloadUri = null
                val b64 = f.optString("b64")
                exec.execute {
                    try {
                        val bytes = android.util.Base64.decode(b64, android.util.Base64.NO_WRAP)
                        contentResolver.openOutputStream(uri)?.use { it.write(bytes) }
                            ?: throw IOException("no output stream")
                        handler.post {
                            refreshStatus("saved ${bytes.size} bytes to phone ✓")
                            Toast.makeText(this, "downloaded ${bytes.size} bytes", Toast.LENGTH_SHORT).show()
                        }
                    } catch (e: Exception) {
                        handler.post { refreshStatus("download failed: ${e.message}") }
                    }
                }
            }
            "FilePutOk" -> {
                val p = f.optString("path")
                refreshStatus("uploaded to host:\n$p")
                Toast.makeText(this, "uploaded → $p", Toast.LENGTH_LONG).show()
            }
            "FileChanged" -> {
                val p = f.optString("path")
                if (openFile == p) {
                    if (draft == openOriginal) reloadFile()      // clean: refresh silently
                    else showConflict()
                }
            }
            "Error" -> {
                when {
                    rid == dirReqId -> { dirReqId = null; refreshStatus("error: ${f.optString("message")}") }
                    rid == readReqId -> { readReqId = null; refreshStatus("error: ${f.optString("message")}") }
                    rid == writeReqId -> {
                        writeReqId = null
                        val msg = f.optString("message")
                        if (msg.startsWith("file changed on disk")) showConflict()
                        else refreshStatus("error: $msg")
                    }
                    rid == downloadReqId -> { downloadReqId = null; refreshStatus("download error: ${f.optString("message")}") }
                }
            }
        }
    }

    // ---- WebView bridge ----
    private inner class JsBridge {
        @JavascriptInterface
        @Suppress("unused")
        fun postMessage(msg: String) {
            try {
                val o = JSONObject(msg)
                when (o.optString("t")) {
                    "ready" -> handler.post {
                        webReady = true
                        applyFontSizes()
                        openFile?.let { pushDoc(draft, Lang.modeForPath(it)) }
                    }
                    "change" -> handler.post {
                        val v = o.optString("value")
                        if (openFile != null && v != draft) {
                            draft = v
                            refreshBar()
                        }
                    }
                }
            } catch (_: Exception) {}
        }
    }

    /**
     * Configure + load the editor page. Fully offline: CodeMirror 5 + the
     * language modes ship in `assets/editor/`. The two `*FromFileURLs` flags
     * are deprecated-but-required for a `file://` page to load its sibling
     * asset scripts and expose the JS bridge — accepted for our local-only,
     * never-loads-remote page (see shouldOverrideUrlLoading above).
     */
    @Suppress("DEPRECATION")
    private fun configureWebView(w: WebView) {
        w.settings.apply {
            javaScriptEnabled = true
            allowFileAccess = true
            allowFileAccessFromFileURLs = true
            allowUniversalAccessFromFileURLs = true
            domStorageEnabled = false
        }
        w.loadUrl("file:///android_asset/editor/editor.html")
    }

    private fun pushDoc(value: String, mode: String) {
        if (!webReady) return
        web.evaluateJavascript("window.__ranch(${jsLit(value)}, ${jsLit(mode)})", null)
    }

    /** Render a String as a valid JS string literal (safe embed in JS). */
    private fun jsLit(s: String): String {
        val sb = StringBuilder("\"")
        for (c in s) {
            when (c) {
                '"' -> sb.append("\\\"")
                '\\' -> sb.append("\\\\")
                '\n' -> sb.append("\\n")
                '\r' -> sb.append("\\r")
                '\t' -> sb.append("\\t")
                '\b' -> sb.append("\\b")
                '\u000C' -> sb.append("\\f")
                else -> if (c.code < 0x20) sb.append(String.format("\\u%04x", c.code)) else sb.append(c)
            }
        }
        return sb.append('"').toString()
    }

    /** Push the persisted font sizes into the editor page as live CSS vars. */
    private fun applyFontSizes() {
        if (!webReady) return
        val code = app.prefs.getInt("editor_font_px", 14)
        val md = app.prefs.getInt("markdown_font_px", 16)
        web.evaluateJavascript("window.__ranchFont($code)", null)
        web.evaluateJavascript("window.__ranchMdFont($md)", null)
    }

    /** Toggle the markdown preview overlay (markdown files only). */
    private fun togglePreview() {
        if (!webReady) return
        previewing = !previewing
        previewBtn.text = if (previewing) "edit" else "preview"
        if (previewing) { saveBtn.visibility = View.GONE; dirtyDot.visibility = View.GONE }
        else refreshBar()
        web.evaluateJavascript("window.__ranchPreview($previewing)", null)
    }

    private fun isMarkdown(p: String): Boolean {
        val n = p.substringAfterLast('/').lowercase()
        return n.endsWith(".md") || n.endsWith(".mdx") || n.endsWith(".markdown")
    }

    // ---- shared helpers ----
    private fun refreshStatus(s: String) {
        statusTv.text = s
        statusTv.setTextColor(0xFF9AA0A6.toInt())
        conflictReload.visibility = View.GONE
        conflictKeep.visibility = View.GONE
    }

    private fun showConflict() {
        statusTv.text = "changed on host — reload or keep your edits?"
        statusTv.setTextColor(0xFFF59E0B.toInt())
        conflictReload.visibility = View.VISIBLE
        conflictKeep.visibility = View.VISIBLE
    }

    private fun clearConflict() {
        statusTv.setTextColor(0xFF9AA0A6.toInt())
        conflictReload.visibility = View.GONE
        conflictKeep.visibility = View.GONE
    }

    private fun copyPath() {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        cm.setPrimaryClip(ClipData.newPlainText("path", pathTv.text.toString()))
        Toast.makeText(this, "copied path", Toast.LENGTH_SHORT).show()
    }

    private fun queryName(uri: Uri): String {
        var name = "upload"
        try {
            contentResolver.query(uri, null, null, null, null)?.use { c ->
                val idx = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                if (idx >= 0 && c.moveToFirst()) name = c.getString(idx)
            }
        } catch (_: Exception) {}
        return name.ifEmpty { uri.lastPathSegment ?: "upload" }
    }

    private fun smallBtn(label: String, click: () -> Unit): Button = Button(this).apply {
        text = label
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
        setTextColor(0xFF7FD4FF.toInt())
        setBackgroundColor(0xFF1B2126.toInt())
        setPadding(dp(10), dp(5), dp(10), dp(5))
        minWidth = 0
        minimumWidth = 0
        setOnClickListener { click() }
    }

    private fun roundedBg(color: Int, radius: Int): android.graphics.drawable.GradientDrawable {
        val bg = android.graphics.drawable.GradientDrawable()
        bg.cornerRadius = radius.toFloat()
        bg.setColor(color)
        return bg
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun optStrList(o: JSONObject, key: String): List<String> =
        o.optJSONArray(key)?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList()

    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(20), dp(20), dp(20))
            setBackgroundColor(0xFF101418.toInt())
            addView(TextView(this@EditorActivity).apply {
                text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            })
            addView(Button(this@EditorActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }

    override fun onDestroy() {
        relay?.removeSink(sink)
        handler.removeCallbacksAndMessages(null)
        exec.shutdownNow()
        // detach + destroy the WebView to avoid leaks / "destroy twice"
        try {
            web.onPause()
            (web.parent as? android.view.ViewGroup)?.removeView(web)
            web.destroy()
        } catch (_: Exception) {}
        super.onDestroy()
    }
}
