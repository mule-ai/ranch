package dev.ranch.android

import android.util.Base64
import org.json.JSONObject
import java.util.UUID

/**
 * Ranch frame builders + shared types for the terminal/session screen.
 * Mirrors the `Attach`/`Input`/`Resize`/`ChatSend`/`SessionsCreate`
 * shapes in `crates/ranch-protocol` and the `b64`/`nextId` helpers in
 * `mobile/lib/frames.ts`.
 */
object Term {
    const val CLIENT = "android-native"
    const val CHAT_TAIL = 25   // chat rows to load on attach (older page in)

    fun newId(): String = UUID.randomUUID().toString()

    fun b64(s: String): String =
        android.util.Base64.encodeToString(s.toByteArray(Charsets.UTF_8), Base64.NO_WRAP)

    // ---- client -> daemon frames ----

    fun hello(): JSONObject =
        JSONObject().put("t", "Hello").put("id", newId()).put("client", CLIENT)

    fun attach(session: String, pane: String? = null): JSONObject {
        val o = JSONObject()
            .put("t", "Attach").put("id", newId()).put("client", CLIENT)
            .put("session", session)
            .put("chat_limit", CHAT_TAIL)
        if (pane != null) o.put("pane", pane)
        return o
    }

    fun detach(): JSONObject =
        JSONObject().put("t", "Detach").put("id", newId()).put("client", CLIENT)

    fun input(session: String, pane: String, text: String): JSONObject =
        JSONObject()
            .put("t", "Input").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane).put("data", b64(text))

    fun resize(session: String, cols: Int, rows: Int): JSONObject =
        JSONObject()
            .put("t", "Resize").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("cols", cols).put("rows", rows)

    fun chatSend(session: String, pane: String, text: String): JSONObject =
        JSONObject()
            .put("t", "ChatSend").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane).put("text", text)

    fun sessionsCreate(kind: String = "shell", name: String? = null): JSONObject {
        val o = JSONObject()
            .put("t", "SessionsCreate").put("req_id", newId()).put("client", CLIENT)
            .put("kind", kind)
        if (name != null) o.put("name", name)
        return o
    }

    // ---- special-key sequences (mirror mobile/screens/Terminal.tsx) ----
    // Escapes use \u00XX so the source stays plain ASCII.

    val KEY_SEQ: Map<String, String> = mapOf(
        "Enter" to "\u000D",
        "Ctrl-C" to "\u0003",
        "Ctrl-D" to "\u0004",
        "Ctrl-L" to "\u000C",
        "Ctrl-R" to "\u0012",
        "Tab" to "\u0009",
        "Esc" to "\u001B",
        "←" to "\u001B[D",
        "↑" to "\u001B[A",
        "↓" to "\u001B[B",
        "→" to "\u001B[C",
        "Back" to "\u007F",
    )

    // ---- daemon -> client types we parse ----

    data class SessionMeta(
        val id: String,
        val name: String,
        val kind: String,
        val activePane: String,
        val panes: List<String>,
    )

    data class ChatMsg(
        val seq: Long,
        val role: String,
        val text: String,
        val toolName: String?,
        val toolCallId: String?,
        val toolOutput: String?,
        val durationMs: Long?,
        val createdAt: String?,
    )

    fun parseSessionMeta(o: JSONObject): SessionMeta =
        SessionMeta(
            id = o.getString("id"),
            name = o.optString("name"),
            kind = o.optString("kind"),
            activePane = o.optString("active_pane"),
            panes = o.optJSONArray("panes")
                ?.let { a -> (0 until a.length()).map { a.getString(it) } }
                ?: emptyList(),
        )

    fun parseSessionList(frame: JSONObject): List<Term.SessionMeta> {
        val arr = frame.optJSONArray("sessions") ?: return emptyList()
        return (0 until arr.length()).map { parseSessionMeta(arr.getJSONObject(it)) }
    }

    fun parseChatMsg(o: JSONObject): ChatMsg =
        ChatMsg(
            seq = o.optLong("seq"),
            role = o.optString("role"),
            text = o.optString("text"),
            toolName = o.optString("tool_name").takeIf { it.isNotEmpty() },
            toolCallId = o.optString("tool_call_id").takeIf { it.isNotEmpty() },
            toolOutput = o.optString("tool_output").takeIf { it.isNotEmpty() },
            durationMs = o.optLong("duration_ms").takeIf { o.has("duration_ms") },
            createdAt = o.optString("created_at").takeIf { it.isNotEmpty() },
        )
}
