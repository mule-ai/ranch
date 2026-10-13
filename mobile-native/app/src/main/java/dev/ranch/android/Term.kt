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

    fun chatSend(session: String, pane: String, text: String, attachments: List<String> = emptyList()): JSONObject {
        val o = JSONObject()
            .put("t", "ChatSend").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane).put("text", text)
        if (attachments.isNotEmpty()) {
            val a = JSONArray()
            attachments.forEach { a.put(it) }
            o.put("attachments", a)
        }
        return o
    }

    fun fileGet(path: String): JSONObject =
        JSONObject().put("t", "FileGet").put("id", newId()).put("client", CLIENT)
            .put("req_id", "fg-" + newId()).put("path", path)

    fun sessionsCreate(
        kind: String = "shell",
        name: String? = null,
        cwd: String? = null,
        reqId: String = "sc-" + newId(),
    ): JSONObject {
        val o = JSONObject()
            .put("t", "SessionsCreate").put("req_id", reqId).put("client", CLIENT)
            .put("kind", kind)
        if (name != null) o.put("name", name)
        if (cwd != null && cwd.isNotEmpty()) o.put("cwd", cwd)
        return o
    }

    // ----- agent builder (Phase B): forge profile CRUD proxy -----
    fun profileList(reqId: String = "pl-" + newId()): JSONObject =
        JSONObject().put("t", "ProfileList").put("req_id", reqId).put("client", CLIENT)

    fun modelCatalog(reqId: String = "mc-" + newId()): JSONObject =
        JSONObject().put("t", "ModelCatalog").put("req_id", reqId).put("client", CLIENT)

    fun profilePut(
        profileId: String? = null,
        name: String,
        provider: String,
        model: String,
        description: String = "",
        baseUrl: String = "",
        apiKey: String = "",
        workingDir: String = "",
        systemPrompt: String = "",
        tools: List<String> = emptyList(),
        reqId: String = "pp-" + newId(),
    ): JSONObject {
        val o = JSONObject()
            .put("t", "ProfilePut").put("req_id", reqId).put("client", CLIENT)
            .put(
                "draft", JSONObject()
                    .put("name", name).put("provider", provider).put("model", model)
            )
        if (profileId != null && profileId.isNotEmpty()) o.put("profile_id", profileId)
        val d = o.getJSONObject("draft")
        if (description.isNotEmpty()) d.put("description", description)
        if (baseUrl.isNotEmpty()) d.put("base_url", baseUrl)
        if (apiKey.isNotEmpty()) d.put("api_key", apiKey)
        if (workingDir.isNotEmpty()) d.put("working_dir", workingDir)
        if (systemPrompt.isNotEmpty()) d.put("system_prompt", systemPrompt)
        if (tools.isNotEmpty()) {
            val arr = org.json.JSONArray()
            tools.forEach { arr.put(it) }
            d.put("tools", arr)
        }
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

    /// Interrupt the in-flight agent turn (immediate, non-destructive).
    /// The agent keeps its session + conversation; the running work stops.
    fun interrupt(session: String, pane: String): JSONObject =
        JSONObject()
            .put("t", "Interrupt").put("id", newId()).put("client", CLIENT)
            .put("session", session).put("pane", pane)

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

    // ---- Herd H1: agent registry (forge agents) ----
    fun agentList(): JSONObject =
        JSONObject().put("t", "AgentList").put("id", newId()).put("client", CLIENT)
            .put("req_id", "al-" + newId())

    fun agentTalk(agent: String, mode: String, reqId: String = "at-" + newId()): JSONObject =
        JSONObject().put("t", "AgentTalk").put("id", newId()).put("client", CLIENT)
            .put("req_id", reqId).put("agent", agent).put("mode", mode)

    /** H6.5: the mobile ＋Agent form → daemon (the `ranch agents new`
     *  wizard's frame twin). Blank fields are omitted; `home_machine`
     *  empty = the daemon defaults to its own relay machine id. */
    fun agentProvision(
        name: String,
        avatar: String = "",
        memoryScope: String = "agent",
        orgId: String = "",
        homeMachine: String = "",
        profileId: String = "",
        wakeCron: String = "",
        wakePrompt: String = "",
        tools: List<String> = emptyList(),
    ): JSONObject {
        val toolsArr = org.json.JSONArray()
        tools.forEach { toolsArr.put(it) }
        return JSONObject()
            .put("t", "AgentProvision").put("req_id", "apv-" + newId())
            .put("name", name)
            .put("memory_scope", memoryScope)
            .put("home_machine", homeMachine)
            .put("tools_allowlist", toolsArr)
            .apply {
                if (avatar.isNotEmpty()) put("avatar", avatar)
                if (orgId.isNotEmpty()) put("org_id", orgId)
                if (profileId.isNotEmpty()) put("profile_id", profileId)
                if (wakeCron.isNotEmpty()) put("wake_cron", wakeCron)
                if (wakePrompt.isNotEmpty()) put("wake_prompt", wakePrompt)
            }
    }

    fun agentBusy(agent: String, reqId: String = "ab-" + newId()): JSONObject =
        JSONObject().put("t", "AgentBusy").put("id", newId()).put("client", CLIENT)
            .put("req_id", reqId).put("agent", agent)

    /** H5.3: pause (or resume) one agent — by id OR registry name — or
     *  the whole herd when [agent] is null. The daemon fans out to
     *  forge `/agents/:id/pause|resume` + mule `/wakes/pause` and
     *  answers `AgentPauseOk` (or `Error` when no agent is known). */
    fun agentPause(agent: String?, paused: Boolean, reqId: String = "ap-" + newId()): JSONObject =
        JSONObject().put("t", "AgentPause").put("id", newId()).put("client", CLIENT)
            .put("req_id", reqId)
            .apply { if (agent != null) put("agent", agent) }
            .put("paused", paused)

    /** Delete a herd agent (forge DELETE /agents/:id). */
    fun agentDelete(agentId: String, reqId: String = "ad-" + newId()): JSONObject =
        JSONObject().put("t", "AgentDelete").put("id", newId()).put("client", CLIENT)
            .put("req_id", reqId).put("agent_id", agentId)

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
    fun dirList(path: String? = null, hidden: Boolean = false): JSONObject {
        val o = JSONObject().put("t", "DirList").put("id", newId()).put("client", CLIENT)
            .put("req_id", "dl-" + newId())
        if (path != null) o.put("path", path)
        if (hidden) o.put("hidden", true)
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

    fun filePut(name: String, b64: String, reqId: String = "fp-" + newId()): JSONObject =
        JSONObject().put("t", "FilePut").put("id", newId()).put("client", CLIENT)
            .put("req_id", reqId).put("name", name).put("b64", b64)

    /// Binary-safe read of ANY file on the daemon host (the IDE "save to
    /// phone" path; distinct from media-only `fileGet`). Replied to with
    /// `FileDownloadOk` (base64, may arrive chunked).
    fun fileDownload(path: String): JSONObject =
        JSONObject().put("t", "FileDownload").put("id", newId()).put("client", CLIENT)
            .put("req_id", "fd-" + newId()).put("path", path)

    /// Move or rename a file/directory. `to` may be in the same dir (rename)
    /// or a different one (move). Replied to with `FileMoveOk` / `Error`.
    fun fileMove(from: String, to: String): JSONObject =
        JSONObject().put("t", "FileMove").put("id", newId()).put("client", CLIENT)
            .put("req_id", "mv-" + newId()).put("from", from).put("to", to)

    /// Copy a file/directory to a new location (source kept in place).
    fun fileCopy(from: String, to: String): JSONObject =
        JSONObject().put("t", "FileCopy").put("id", newId()).put("client", CLIENT)
            .put("req_id", "cp-" + newId()).put("from", from).put("to", to)

    /// Delete a file or directory (recursive). The caller confirms first.
    fun fileDelete(path: String): JSONObject =
        JSONObject().put("t", "FileDelete").put("id", newId()).put("client", CLIENT)
            .put("req_id", "del-" + newId()).put("path", path)

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
        val cwd: String? = null,
        val busy: Boolean = false,
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
        val attachments: List<String>?,
        val imageRefs: List<String>?,
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

    /// One row of the forge agent registry (Herd H1). Mirrors
    /// `ranch_protocol::AgentInfo`.
    data class AgentInfo(
        val id: String,
        val name: String,
        val avatarUrl: String?,
        val homeMachine: String?,
        val lastActive: String?,
        val conversationCount: Long?,
    )

    fun parseAgentInfo(o: JSONObject): AgentInfo =
        AgentInfo(
            id = o.optString("id"),
            name = o.optString("name"),
            avatarUrl = o.optString("avatar_url").takeIf { it.isNotEmpty() },
            homeMachine = o.optString("home_machine").takeIf { it.isNotEmpty() },
            lastActive = o.optString("last_active").takeIf { it.isNotEmpty() },
            conversationCount =
                if (o.has("conversation_count") && !o.isNull("conversation_count"))
                    o.optLong("conversation_count") else null,
        )

    /** One row of the Activity View (Herd H5.3) — mirrors
     *  `ranch_protocol::ActivityItem`. [agent] is the registry name;
     *  null = machine-wide item (local ask/spawn/trigger/mule_job). */
    data class ActivityItem(
        val id: String,
        val kind: String, // ask|spawn|trigger|mule_job|forge_task|belief_pending|research_open|wake
        val agent: String?,
        val text: String,
        val ts: String, // RFC 3339 (UTC Z)
    )

    fun parseActivityItem(o: JSONObject): ActivityItem =
        ActivityItem(
            id = o.optString("id"),
            kind = o.optString("kind"),
            agent = o.optString("agent").takeIf { it.isNotEmpty() },
            text = o.optString("text"),
            ts = o.optString("ts"),
        )

    /** One agent's pause/resume outcome (H5.3) — mirrors
     *  `ranch_protocol::AgentPauseResult`. [forge] = resulting paused
     *  state (null = leg not attempted); [mule] = flips (null = same). */
    data class AgentPauseResult(
        val agent: String,
        val name: String,
        val forge: Boolean?,
        val mule: Int?,
        val error: String?,
    )

    fun parseAgentPauseResult(o: JSONObject): AgentPauseResult =
        AgentPauseResult(
            agent = o.optString("agent"),
            name = o.optString("name"),
            forge = if (o.has("forge") && !o.isNull("forge")) o.optBoolean("forge") else null,
            mule = if (o.has("mule") && !o.isNull("mule")) o.optInt("mule") else null,
            error = o.optString("error").takeIf { it.isNotEmpty() },
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
            cwd = o.optString("cwd").takeIf { it.isNotEmpty() },
            busy = o.optBoolean("busy", false),
        )

    fun parseSessionList(frame: JSONObject): List<Term.SessionMeta> {
        val arr = frame.optJSONArray("sessions") ?: return emptyList()
        return (0 until arr.length()).map { parseSessionMeta(arr.getJSONObject(it)) }
    }

    private fun strList(o: JSONObject, key: String): List<String>? {
        val a = o.opt(key) ?: return null
        if (a !is JSONArray) return null
        val out = (0 until a.length()).mapNotNull { a.optString(it).takeIf { s -> s.isNotEmpty() } }
        return out.takeIf { it.isNotEmpty() }
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
            attachments = strList(o, "attachments"),
            imageRefs = strList(o, "image_refs"),
        )
}
