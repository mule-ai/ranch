package dev.ranch.android

/**
 * SGR (Select Graphic Rendition) row parser — Kotlin port of
 * `mobile/lib/sgr.ts`. The daemon's VT formatter emits one grid row per
 * string with inline SGR sequences (colors, bold, ...) embedded. We split a
 * row into styled [SgrSpan]s for the terminal renderer.
 *
 * Colors are Android ARGB ints (0xFFRRGGBB); null = "use the view default".
 */
data class SgrSpan(
    val text: String,
    val fg: Int?,
    val bg: Int?,
    val bold: Boolean,
    val italic: Boolean,
    val underline: Boolean,
)

object Sgr {
    private fun hex(s: String): Int {
        val r = s.substring(1, 3).toInt(16)
        val g = s.substring(3, 5).toInt(16)
        val b = s.substring(5, 7).toInt(16)
        return 0xFF000000.toInt() or (r shl 16) or (g shl 8) or b
    }

    private fun rgb(r: Int, g: Int, b: Int): Int =
        0xFF000000.toInt() or (r shl 16) or (g shl 8) or b

    private val NAMED: Map<Int, Int> = mapOf(
        30 to hex("#3b3b3b"), 31 to hex("#cd3231"), 32 to hex("#00bc00"), 33 to hex("#949494"),
        34 to hex("#0451a5"), 35 to hex("#bc05bc"), 36 to hex("#0598bc"), 37 to hex("#555555"),
        90 to hex("#7f7f7f"), 91 to hex("#cd3131"), 92 to hex("#14cc14"), 93 to hex("#f5f543"),
        94 to hex("#3b78ff"), 95 to hex("#d670d6"), 96 to hex("#00a0a0"), 97 to hex("#e5e5e5"),
        40 to hex("#3b3b3b"), 41 to hex("#cd3231"), 42 to hex("#00bc00"), 43 to hex("#949494"),
        44 to hex("#0451a5"), 45 to hex("#bc05bc"), 46 to hex("#0598bc"), 47 to hex("#555555"),
        100 to hex("#7f7f7f"), 101 to hex("#cd3131"), 102 to hex("#14cc14"), 103 to hex("#f5f543"),
        104 to hex("#3b78ff"), 105 to hex("#d670d6"), 106 to hex("#00a0a0"), 107 to hex("#e5e5e5"),
    )

    // xterm 256-color palette (16 base + 6x6x6 cube + 24 grays), in the
    // same order as sgr.ts so index 0..255 line up.
    private val PALETTE: IntArray = run {
        val p = ArrayList<Int>(256)
        listOf(
            "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
            "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
        ).forEach { p.add(hex(it)) }
        val steps = intArrayOf(0, 95, 135, 175, 215, 255)
        for (r in steps) for (g in steps) for (b in steps) p.add(rgb(r, g, b))
        for (i in 0 until 24) { val v = 8 + i * 10; p.add(rgb(v, v, v)) }
        p.toIntArray()
    }

    private fun palette(idx: Int): Int =
        if (idx in 0 until PALETTE.size) PALETTE[idx] else rgb(204, 204, 204)

    fun parseRow(line: String): List<SgrSpan> {
        val spans = ArrayList<SgrSpan>()
        var fg: Int? = null
        var bg: Int? = null
        var bold = false
        var italic = false
        var underline = false
        var text = ""

        fun flush() {
            if (text.isNotEmpty()) {
                spans.add(SgrSpan(text, fg, bg, bold, italic, underline))
                text = ""
            }
        }

        fun applySgr(params: IntArray) {
            var i = 0
            while (i < params.size) {
                val p = params[i]
                when (p) {
                    0 -> { fg = null; bg = null; bold = false; italic = false; underline = false }
                    1 -> bold = true
                    2 -> bold = false   // faint -> treat as normal
                    3 -> italic = true
                    4 -> underline = true
                    22 -> bold = false
                    23 -> italic = false
                    24 -> underline = false
                    39 -> fg = null
                    49 -> bg = null
                    38, 48 -> {
                        val isFg = p == 38
                        if (i + 2 < params.size && params[i + 1] == 5) {
                            val c = palette(params[i + 2])
                            if (isFg) fg = c else bg = c
                            i += 2
                        } else if (i + 4 < params.size && params[i + 1] == 2) {
                            val c = rgb(params[i + 2].coerceIn(0, 255), params[i + 3].coerceIn(0, 255), params[i + 4].coerceIn(0, 255))
                            if (isFg) fg = c else bg = c
                            i += 4
                        }
                    }
                    else -> NAMED[p]?.let { c ->
                        when (p) {
                            in 40..47 -> bg = c
                            in 100..107 -> bg = c
                            else -> fg = c
                        }
                    }
                }
                i++
            }
        }

        var i = 0
        val n = line.length
        while (i < n) {
            val ch = line[i]
            if (ch == '\u001b' && i + 1 < n && line[i + 1] == '[') {
                var j = i + 2
                val sb = StringBuilder()
                while (j < n) {
                    val c = line[j]
                    if (c in '0'..'9' || c == ';') { sb.append(c); j++ } else break
                }
                if (j < n && line[j] == 'm') {
                    flush()
                    val params = if (sb.isEmpty()) intArrayOf(0)
                    else sb.toString().split(';').map { it.toIntOrNull() ?: 0 }.toIntArray()
                    applySgr(params)
                    i = j + 1
                    continue
                } else {
                    // other CSI (cursor moves etc.) — skip the whole sequence
                    flush()
                    while (j < n) { val c = line[j]; if (c in '@'..'~') break; j++ }
                    i = j + 1
                    continue
                }
            }
            text += ch
            i++
        }
        flush()
        return spans
    }
}
