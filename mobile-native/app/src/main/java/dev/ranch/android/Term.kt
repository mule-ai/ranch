package dev.ranch.android

import android.util.Base64
import org.json.JSONArray
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

    fun agentAskAnswer(askId: String, choices: List<Int>, text: String): JSONObject =
        JSONObject()
            .put("t", "AgentAskAnswer")
            .put("ask_id", askId)
            .put("choices", JSONArray(choices))
            .put("text", text)

    fun modelList(pane: String): JSONObject =
        JSONObject()
            .put("t", "ModelList").put("id", newId()).put("client", CLIENT)
            .put("pane", pane).put("req_id", "ml-" + newId())

    fun modelSet(session: String, pane: String, provider: String, model: String): JSONObject =
        JSONObject()
            .put("t", "ModelSet").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane)
            .put("provider", provider).put("model", model)
            .put("req_id", "ms-" + newId())

    fun chatCompact(session: String, pane: String): JSONObject =
        JSONObject()
            .put("t", "ChatCompact").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane)
            .put("req_id", "compact-" + newId())

    fun chatHistory(session: String, pane: String, limit: Int, before: Long? = null): JSONObject {
        val o = JSONObject()
            .put("t", "ChatHistory").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane)
            .put("req_id", "ch-" + newId()).put("limit", limit)
        if (before != null) o.put("before", before)
        return o
    }

    fun scrollbackReq(session: String, pane: String, offset: Long = 0, limit: Int = 2000): JSONObject =
        JSONObject()
            .put("t", "ScrollbackReq").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane)
            .put("offset", offset).put("limit", limit)

    fun sessionsKill(session: String): JSONObject =
        JSONObject().put("t", "SessionsKill").put("session", session)

    fun sessionsRename(session: String, name: String): JSONObject =
        JSONObject().put("t", "SessionsRename").put("session", session).put("name", name)

    fun upgrade(): JSONObject = JSONObject().put("t", "Upgrade")

    // ---- Phase 4: agents / pi manager ----
    fun piList(): JSONObject =
        JSONObject().put("t", "PiList").put("id", newId()).put("client", CLIENT)
            .put("req_id", "pil-" + newId())

    fun piMonitor(enabled: Boolean): JSONObject =
        JSONObject().put("t", "PiMonitor").put("enabled", enabled).put("req_id", "pim-" + newId())

    fun adoptPiSession(sessionFile: String): JSONObject {
        val o = JSONObject().put("t", "SessionsCreate").put("req_id", newId())
            .put("client", CLIENT).put("kind", "pi")
        o.put("pi_session_file", sessionFile)
        return o
    }

    // ---- Phase 4: file editor ----
    fun dirList(path: String? = null): JSONObject {
        val o = JSONObject().put("t", "DirList").put("id", newId()).put("client", CLIENT)
            .put("req_id", "dl-" + newId())
        if (path != null) o.put("path", path)
        return o
    }

    fun fileRead(path: String): JSONObject =
        JSONObject().put("t", "FileRead").put("id", newId()).put("client", CLIENT)
            .put("req_id", "fr-" + newId()).put("path", path)

    fun fileWrite(path: String, content: String, mtime: Long? = null): JSONObject {
        val o = JSONObject().put("t", "FileWrite").put("id", newId()).put("client", CLIENT)
            .put("req_id", "fw-" + newId()).put("path", path).put("content", content)
        if (mtime != null) o.put("mtime", mtime)
        return o
    }

    fun filePut(name: String, b64: String): JSONObject =
        JSONObject().put("t", "FilePut").put("id", newId()).put("client", CLIENT)
            .put("req_id", "fp-" + newId()).put("name", name).put("b64", b64)

    // ---- Phase 4: workflows / mule ----
    fun workflowList(): JSONObject =
        JSONObject().put("t", "WorkflowList").put("req_id", "wl-" + newId())

    fun workflowGet(id: String): JSONObject =
        JSONObject().put("t", "WorkflowGet").put("req_id", "wg-" + newId()).put("workflow", id)

    fun workflowRun(id: String): JSONObject =
        JSONObject().put("t", "WorkflowRun").put("req_id", "wr-" + newId()).put("workflow", id)

    fun workflowDelete(id: String): JSONObject =
        JSONObject().put("t", "WorkflowDelete").put("req_id", "wd-" + newId()).put("workflow", id)

    fun muleAgents(): JSONObject =
        JSONObject().put("t", "MuleAgents").put("req_id", "ma-" + newId())

    // ---- Phase 4: triggers ----
    fun triggerList(): JSONObject =
        JSONObject().put("t", "TriggerList").put("req_id", "tl-" + newId())

    fun triggerRun(id: String): JSONObject =
        JSONObject().put("t", "TriggerRun").put("req_id", "tr-" + newId()).put("trigger", id)

    fun triggerDelete(id: String): JSONObject =
        JSONObject().put("t", "TriggerDelete").put("req_id", "td-" + newId()).put("trigger", id)

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
        val toolArgs: String?,
        val durationMs: Long?,
        val createdAt: String?,
    )

    data class ModelChoice(
        val provider: String,
        val id: String,
        val name: String,
    )

    data class AgentAsk(
        val askId: String,
        val session: String,
        val pane: String,
        val question: String,
        val choices: List<String>,
        val suggested: Int?,
        val multi: Boolean,
        val freeText: Boolean,
    )

    data class PiSession(
        val id: String,
        val title: String,
        val sessionFile: String,
        val path: String,
        val active: Boolean,
        val external: Boolean,
        val updated: String,
    )

    fun parsePiSession(o: JSONObject): PiSession =
        PiSession(
            id = o.optString("id"),
            title = o.optString("title"),
            sessionFile = o.optString("session_file"),
            path = o.optString("path"),
            active = o.optBoolean("active", false),
            external = o.optBoolean("external", false),
            updated = o.optString("updated"),
        )

    fun parseModelChoice(o: JSONObject): ModelChoice =
        ModelChoice(
            provider = o.optString("provider"),
            id = o.optString("id"),
            name = o.optString("name"),
        )

    fun parseAgentAsk(f: JSONObject): AgentAsk =
        AgentAsk(
            askId = f.optString("ask_id"),
            session = f.optString("session"),
            pane = f.optString("pane"),
            question = f.optString("question"),
            choices = f.optJSONArray("choices")?.let { a -> (0 until a.length()).map { a.optString(it) } } ?: emptyList(),
            suggested = if (f.has("suggested") && !f.isNull("suggested")) f.optInt("suggested") else null,
            multi = f.optBoolean("multi", false),
            freeText = f.optBoolean("free_text", true),
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
            toolArgs = o.optString("tool_args").takeIf { it.isNotEmpty() },
            durationMs = o.optLong("duration_ms").takeIf { o.has("duration_ms") },
            createdAt = o.optString("created_at").takeIf { it.isNotEmpty() },
        )
}
