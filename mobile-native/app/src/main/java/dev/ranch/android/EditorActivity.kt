package dev.ranch.android

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.Base64
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONObject
import java.io.File

/**
 * Phase 4 — File editor.
 * Browse the daemon host's filesystem with `DirList`/`DirListOk`
 * (dirs + files + parent), open a file with `FileRead`/`FileReadOk`,
 * edit and save with `FileWrite`/`FileWriteOk` (mtime conflict check),
 * and push a local phone file up with `FilePut`/`FilePutOk` (base64).
 * `FileChanged` (external modification) triggers a re-read notice.
 */
class EditorActivity : Activity() {

    private val handler = Handler(Looper.getMainLooper())
    private var relay: RelaySession? = null
    private lateinit var sink: (JSONObject) -> Unit
    private lateinit var status: TextView
    private lateinit var pathBar: TextView
    private lateinit var fileBox: LinearLayout   // dir listing OR editor
    private var curPath: String? = null
    private var curFile: String? = null
    private var curMtime: Long? = null

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
            text = "Files"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTextColor(0xFFE5E5E5.toInt()); gravity = Gravity.CENTER_VERTICAL; setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "browse the daemon host"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(0xFF9aa0a6.toInt())
        }
        root.addView(status)
        pathBar = TextView(this).apply {
            text = ""; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setTextColor(0xFF7fd4ff.toInt())
        }
        root.addView(pathBar)

        // start dir + upload button
        val startRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val startPath = EditText(this).apply {
            hint = "start dir (blank = home)"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
        }
        startRow.addView(startPath, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        startRow.addView(Button(this).apply {
            text = "Go"; setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener { browse(startPath.text.toString().trim().ifEmpty { null }) }
        })
        startRow.addView(Button(this).apply {
            text = "Upload"; setPadding(dp(6), 0, dp(6), 0)
            setOnClickListener { uploadLast() }
        })
        root.addView(startRow)

        fileBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(ScrollView(this).apply { addView(fileBox) },
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))
        sink = { f -> handler.post { onFrame(f) } }
        r.addSink(sink)
        browse(null)
    }

    override fun onDestroy() {
        relay?.removeSink(sink); handler.removeCallbacksAndMessages(null); super.onDestroy()
    }

    // ---- browse ----
    private fun browse(path: String?) {
        curFile = null; curMtime = null
        status.text = "listing…"
        relay?.send(Term.dirList(path))
    }

    private fun onDirListOk(f: JSONObject) {
        curPath = f.optString("path")
        pathBar.text = curPath
        val parent = f.optString("parent")
        val dirs = f.optJSONArray("dirs")?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList()
        val files = f.optJSONArray("files")?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList()
        val box = fileBox
        box.removeAllViews()
        if (parent.isNotEmpty()) {
            box.addView(itemBtn("⌂ ..") { browse(parent) })
        }
        for (d in dirs) box.addView(itemBtn("📁 $d") { browse("$curPath/$d") })
        for (fl in files) box.addView(itemBtn("📄 $fl") { openFile("$curPath/$fl") })
        if (dirs.isEmpty() && files.isEmpty())
            box.addView(TextView(this).apply { text = "(empty)"; setTextColor(0xFF6b7280.toInt()) })
        status.text = "${dirs.size} dirs, ${files.size} files"
    }

    private fun itemBtn(label: String, onClick: () -> Unit): Button =
        Button(this).apply {
            text = label; setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFd1d5db.toInt())
            setPadding(dp(8), dp(6), dp(8), dp(6))
            setOnClickListener { onClick() }
        }

    // ---- read / edit / write ----
    private fun openFile(path: String) {
        curFile = path
        status.text = "reading…"
        relay?.send(Term.fileRead(path))
    }

    private fun onFileReadOk(f: JSONObject) {
        val path = f.optString("path"); curFile = path
        curMtime = f.optLong("mtime")
        val content = f.optString("content")
        pathBar.text = path
        fileBox.removeAllViews()
        fileBox.addView(TextView(this).apply {
            text = "editing — ${f.optLong("size")} bytes"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(0xFF9aa0a6.toInt()); setPadding(dp(4), 0, dp(4), dp(4))
        })
        val edit = EditText(this).apply {
            setText(content)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTypeface(android.graphics.Typeface.MONOSPACE)
            setBackgroundColor(0xFF1a1b23.toInt())
            setPadding(dp(8), dp(8), dp(8), dp(8))
        }
        fileBox.addView(edit)
        val actions = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        actions.addView(Button(this).apply {
            text = "Save"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setPadding(dp(10), dp(4), dp(10), dp(4))
            setOnClickListener {
                relay?.send(Term.fileWrite(path, edit.text.toString(), curMtime))
                status.text = "saving…"
            }
        })
        actions.addView(Button(this).apply {
            text = "Back"; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f); setPadding(dp(10), dp(4), dp(10), dp(4))
            setOnClickListener { browse(null) }
        })
        fileBox.addView(actions)
        status.text = "loaded"
    }

    // ---- upload a local phone file ----
    private var lastUploadName = ""
    private fun uploadLast() {
        // pick from app-specific external dir (no SAF picker for MVP):
        // the user places a file at /sdcard/Download/ranch-upload/<name>
        val dir = android.os.Environment.getExternalStoragePublicDirectory(
            android.os.Environment.DIRECTORY_DOWNLOADS)
        val f = File(dir, "ranch-upload")
        if (!f.exists()) {
            status.text = "put a file in /sdcard/Download/ranch-upload/ first"
            return
        }
        val pick = f.listFiles()?.firstOrNull()
        if (pick == null) { status.text = "ranch-upload/ is empty"; return }
        val b64 = Base64.encodeToString(pick.readBytes(), Base64.NO_WRAP)
        lastUploadName = pick.name
        relay?.send(Term.filePut(pick.name, b64))
        status.text = "uploading ${pick.name} (${pick.length()} bytes)…"
    }

    // ---- frame dispatch ----
    private fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "DirListOk" -> onDirListOk(f)
            "FileReadOk" -> onFileReadOk(f)
            "FileWriteOk" -> status.text = "saved ${f.optString("path")}"
            "FilePutOk" -> status.text = "uploaded ${f.optString("path")} (${f.optLong("size")} bytes)"
            "FileChanged" -> {
                val p = f.optString("path")
                if (p == curFile) status.text = "changed on host — tap Reload"; reload()
            }
            "Error" -> status.text = "error: ${f.optString("message")}"
        }
    }

    private fun reload() {
        curFile?.let { openFile(it) }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()
    private fun errorView(msg: String): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(TextView(this@EditorActivity).apply { text = msg; setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f) })
            addView(Button(this@EditorActivity).apply { text = "Back"; setOnClickListener { finish() } })
        }
}
