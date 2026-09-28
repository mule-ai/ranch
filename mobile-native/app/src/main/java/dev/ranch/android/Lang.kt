package dev.ranch.android

/**
 * Map a file path to a CodeMirror 5 mode for the editor WebView.
 * Mirrors `mobile/lib/highlight.ts` (`langForPath`) + `Editor.tsx` (`cmMode`).
 * Unknown extensions → "null" (no highlighting; the file still edits fine).
 */
object Lang {
    // extension (lowercased) -> language
    private val EXT_LANG = mapOf(
        "rs" to "rust",
        "c" to "c", "h" to "c",
        "cpp" to "cpp", "cc" to "cpp", "cxx" to "cpp", "hpp" to "cpp",
        "go" to "go",
        "java" to "java",
        "cs" to "csharp",
        "js" to "js", "mjs" to "js", "cjs" to "js", "jsx" to "jsx",
        "ts" to "ts", "tsx" to "tsx",
        "py" to "python",
        "sh" to "shell", "bash" to "shell", "zsh" to "shell",
        "json" to "json", "json5" to "json",
        "toml" to "toml",
        "yaml" to "yaml", "yml" to "yaml",
        "md" to "markdown", "mdx" to "markdown",
        "html" to "html", "htm" to "html",
        "css" to "css",
        "xml" to "xml",
        "txt" to "plain", "log" to "plain",
    )

    // language -> CodeMirror mode
    private val CM_MODE = mapOf(
        "rust" to "rust",
        "c" to "text/x-csrc",
        "cpp" to "text/x-c++src",
        "csharp" to "text/x-csharp",
        "go" to "text/x-go",
        "java" to "text/x-java",
        "js" to "text/javascript",
        "jsx" to "text/javascript",
        "ts" to "application/typescript",
        "tsx" to "application/typescript",
        "python" to "text/x-python",
        "shell" to "text/x-sh",
        "json" to "application/json",
        "toml" to "text/x-toml",
        "yaml" to "text/x-yaml",
        "markdown" to "markdown",
        "html" to "htmlmixed",
        "css" to "text/css",
        "xml" to "application/xml",
        "plain" to "null",
    )

    fun langForPath(path: String): String {
        val base = path.substringAfterLast('/')
        val dot = base.lastIndexOf('.')
        if (dot < 0) return "plain"
        val ext = base.substring(dot + 1).lowercase()
        return EXT_LANG[ext] ?: "plain"
    }

    fun modeForPath(path: String): String = CM_MODE[langForPath(path)] ?: "null"

    /** True for code files that get real highlighting (vs markdown/plain). */
    fun isCode(path: String): Boolean {
        val l = langForPath(path)
        return l != "plain" && l != "markdown"
    }

    /** Best-effort MIME from the extension (for SAF save-dialog hints). */
    fun guessMime(path: String): String {
        val ext = path.substringAfterLast('/', "").substringAfterLast('.', "").lowercase()
        return when (ext) {
            "png" -> "image/png"; "jpg", "jpeg" -> "image/jpeg"; "gif" -> "image/gif"
            "webp" -> "image/webp"; "bmp" -> "image/bmp"; "svg" -> "image/svg+xml"
            "pdf" -> "application/pdf"; "zip" -> "application/zip"; "gz" -> "application/gzip"
            "json" -> "application/json"; "toml" -> "application/toml"
            "txt", "log" -> "text/plain"; "md", "mdx" -> "text/markdown"
            "html", "htm" -> "text/html"; "csv" -> "text/csv"
            else -> "application/octet-stream"
        }
    }
}
