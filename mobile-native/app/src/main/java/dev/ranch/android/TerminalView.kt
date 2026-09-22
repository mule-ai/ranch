package dev.ranch.android

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import android.os.Handler
import android.os.Looper
import android.view.View

/**
 * Renders one pane's terminal grid (a set of SGR-tagged row strings) into a
 * fixed cell grid. The daemon owns the emulation; this view only paints what
 * the [Snapshot]/[Update] frames describe. See mobile/screens/Terminal.tsx
 * for the reference implementation.
 *
 * Not a true multi-pane split renderer: one pane fills the view. Multi-pane
 * sessions are switched via the pane tabs in the host activity.
 */
class TerminalView(context: Context) : View(context) {

    // ---- screen data (source of truth for drawing is [rowSpans]) ----
    @Volatile var cols: Int = 80
        private set
    @Volatile var rows: Int = 24
        private set
    private val rawLines = ArrayList<String>()
    @Volatile private var rowSpans: Array<Array<SgrSpan>> = arrayOf()
    @Volatile private var cursorX = 0
    @Volatile private var cursorY = 0
    @Volatile private var cursorVisible = true

    /** Set true when this pane is the one receiving input (drives cursor blink). */
    var focused: Boolean = false
        set(value) {
            if (field == value) return
            field = value
            if (value) startBlink() else stopBlink()
            invalidate()
        }

    // ---- predictive echo (client-side) ----
    @Volatile private var predText = ""
    @Volatile private var predX = 0
    @Volatile private var predY = 0

    /**
     * Paint [text] faintly at cell (x,y) until the next real [applyUpdate]
     * replaces that row (which clears the prediction). Reduces perceived
     * latency for local keystrokes.
     */
    fun setPrediction(x: Int, y: Int, text: String) {
        predText = text
        predX = x
        predY = y
        invalidate()
    }

    /** Reports the cell geometry the view can display (cols, rows). */
    var onGeometry: ((Int, Int) -> Unit)? = null

    // ---- metrics / paint ----
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply { isFakeBoldText = false }
    private val tfNormal = Typeface.create("monospace", Typeface.NORMAL)
    private val tfBold = Typeface.create("monospace", Typeface.BOLD)
    private var cellW = 0
    private var cellH = 0
    private var baseline = 0

    private val defaultBg = Color.parseColor("#151a1e")
    private val defaultFg = Color.parseColor("#d8dee2")
    private val cursorHi = Color.parseColor("#4a6b8a")
    private val bgPaint = Paint()
    private val cursorPaint = Paint()

    // ---- cursor blink ----
    private val handler = Handler(Looper.getMainLooper())
    @Volatile private var blinkOn = true
    private val blinkRunnable = object : Runnable {
        override fun run() {
            blinkOn = !blinkOn
            invalidate()
            if (focused) handler.postDelayed(this, 530)
        }
    }

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        setBackgroundColor(defaultBg)
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        if (w <= 0 || h <= 0) return
        // base font size in px (13sp)
        val density = resources.displayMetrics.density
        val fontSize = (13 * density)
        paint.textSize = fontSize.toFloat()
        val fm = paint.fontMetrics
        cellH = (fm.descent - fm.ascent).toInt() + 1
        cellW = paint.measureText("W").toInt().coerceAtLeast(1)
        baseline = (-fm.ascent).toInt()
        val newCols = (w / cellW).coerceAtLeast(4)
        val newRows = (h / cellH).coerceAtLeast(2)
        if (newCols != cols || newRows != rows) {
            cols = newCols
            rows = newRows
            onGeometry?.invoke(cols, rows)
        }
        invalidate()
    }

    /** Replace the whole screen (from a Snapshot). [cursor] = (x,y,visible)?. */
    fun setScreen(cols: Int, rows: Int, lines: List<String>, cursor: Triple<Int, Int, Boolean>?) {
        this.cols = cols
        this.rows = rows
        val raw = ArrayList<String>(rows)
        for (i in 0 until rows) raw.add(if (i < lines.size) lines[i] else "")
        rawLines.clear()
        rawLines.addAll(raw)
        val spans = Array(rows) { Sgr.parseRow(raw[it]).toTypedArray() }
        rowSpans = spans
        applyCursor(cursor)
        invalidate()
    }

    /** Apply incremental row changes (from an Update). */
    fun applyUpdate(rowsUpd: List<Pair<Int, String>>, cursor: Triple<Int, Int, Boolean>?) {
        val spans = rowSpans
        // the real frame arrived — clear any predictive echo it supersedes
        predText = ""
        for ((y, text) in rowsUpd) {
            if (y in 0 until spans.size) {
                spans[y] = Sgr.parseRow(text).toTypedArray()
                if (y < rawLines.size) rawLines[y] = text else while (rawLines.size <= y) rawLines.add("")
            }
        }
        applyCursor(cursor)
        invalidate()
    }

    private fun applyCursor(c: Triple<Int, Int, Boolean>?) {
        if (c != null) {
            cursorX = c.first
            cursorY = c.second
            cursorVisible = c.third
        }
    }

    private fun startBlink() {
        blinkOn = true
        handler.removeCallbacks(blinkRunnable)
        handler.postDelayed(blinkRunnable, 530)
    }

    private fun stopBlink() {
        handler.removeCallbacks(blinkRunnable)
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        if (focused) startBlink()
    }

    override fun onDetachedFromWindow() {
        stopBlink()
        super.onDetachedFromWindow()
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val rows = rowSpans.size
        val cw = cellW
        val ch = cellH

        // background
        canvas.drawRect(0f, 0f, (cw * cols).toFloat(), (ch * rows).toFloat(), bgPaint.apply { color = defaultBg })

        val showCursor = focused && cursorVisible && blinkOn

        for (y in 0 until rows) {
            val spans = rowSpans[y]
            var x = 0
            for (span in spans) {
                val len = span.text.length
                if (len == 0) continue
                val bg = span.bg
                if (bg != null) {
                    val x0 = (x * cw).toFloat()
                    canvas.drawRect(x0, (y * ch).toFloat(), x0 + len * cw, (y + 1).toFloat() * ch, bgPaint.apply { color = bg })
                }
                paint.typeface = if (span.bold) tfBold else tfNormal
                paint.isFakeBoldText = span.bold
                paint.color = span.fg ?: defaultFg
                val tx = (x * cw).toFloat()
                val ty = (y * ch + baseline).toFloat()
                canvas.drawText(span.text, tx, ty, paint)
                if (span.underline) {
                    val uy = (y + 1).toFloat() * ch - 1f
                    canvas.drawRect((x * cw).toFloat(), uy, (x + len).toFloat() * cw, uy + 1f, bgPaint.apply { color = span.fg ?: defaultFg })
                }
                x += len
            }

            // cursor block on this row
            if (showCursor && y == cursorY && cursorX < cols) {
                val cx = cursorX * cw
                canvas.drawRect(cx.toFloat(), (y * ch).toFloat(), (cx + cw).toFloat(), (y + 1).toFloat() * ch, cursorPaint.apply { color = cursorHi })
                // redraw the char under the cursor in a light color for legibility
                val cellChar = if (cursorY < rawLines.size && cursorX < rawLines[cursorY].length) rawLines[cursorY][cursorX] else ' '
                paint.typeface = tfNormal
                paint.isFakeBoldText = false
                paint.color = Color.WHITE
                canvas.drawText(cellChar.toString(), cx.toFloat(), (y * ch + baseline).toFloat(), paint)
            }

            // predictive echo: dim chars the user typed but the daemon hasn't
            // echoed back yet
            if (predText.isNotEmpty() && y == predY) {
                paint.typeface = tfNormal
                paint.isFakeBoldText = false
                paint.color = Color.parseColor("#5a6268")
                for ((ix, chx) in predText.withIndex()) {
                    val px = (predX + ix) * cw
                    if (px >= cols * cw) break
                    canvas.drawText(chx.toString(), px.toFloat(), (y * ch + baseline).toFloat(), paint)
                }
            }
        }
    }
}
