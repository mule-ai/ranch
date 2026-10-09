package dev.ranch.android

import java.security.MessageDigest

/**
 * H6.1 — shared notification POLICY (pure, no Android deps).
 *
 * SOURCE OF TRUTH: `mobile/lib/notify-policy.ts` (the Expo/TS twin). The
 * two files must stay in sync — if you change a kind, a default, a lane
 * fingerprint, or an action label there, change it here too (and vice
 * versa). There is no cross-runtime code sharing between the two apps
 * (JVM vs JS); this mirror is deliberately tiny so drift is eyeballable.
 *
 * Kinds and defaults (both apps):
 *   turn_end     true   pane working -> idle edge (existing, re-tagged
 *                       with the agent name when resolvable)
 *   errors       true   `⚠` assistant chat rows (existing)
 *   questions    true   ad-hoc AgentAsk with no recognizable card lane
 *   approval     true   policy_ask card ("Allow this action" / "Deny")
 *   research     true   research_report card ("Use" / "Discard" / "Ask more")
 *   memory       true   memory_review card ("Keep" / "Forget")
 *   task_done    true   a forge_task activity item left the ActivityPush
 *                       set (its state went terminal)
 *   every_message false (existing; only when turn_end is off)
 *
 * Mute/priority: `muted` suppresses notifications ONLY — the agent's
 * items still appear in the Activity View (a separate surface).
 * `quiet` keeps the notification but silences it (low-importance
 * channel); `high` routes it to a high-importance channel.
 */
object NotifyPolicy {
    // ---- kinds & defaults (mirror notify-policy.ts KIND_DEFAULTS) ----
    const val TURN_END = "turn_end"
    const val ERRORS = "errors"
    const val QUESTIONS = "questions"
    const val APPROVAL = "approval"
    const val RESEARCH = "research"
    const val MEMORY = "memory"
    const val TASK_DONE = "task_done"
    const val EVERY_MESSAGE = "every_message"

    val KIND_DEFAULTS: Map<String, Boolean> = mapOf(
        TURN_END to true,
        ERRORS to true,
        QUESTIONS to true,
        APPROVAL to true,
        RESEARCH to true,
        MEMORY to true,
        TASK_DONE to true,
        EVERY_MESSAGE to false,
    )

    /** Kinds whose notification carries actionable buttons. */
    val ACTIONABLE = setOf(QUESTIONS, APPROVAL, RESEARCH, MEMORY)

    // ---- per-agent settings ----
    data class AgentPrefs(val muted: Boolean = false, val priority: String = "normal") {
        val valid: Boolean get() = priority in setOf("high", "normal", "quiet")
    }

    /** kind enabled (user toggle) AND agent not muted → notify. */
    fun decision(kindEnabled: Boolean, agentPrefs: AgentPrefs?): Boolean =
        kindEnabled && agentPrefs?.muted != true

    // ---- ask-lane classification (mirror laneForChoices) ----
    // The AgentAskRequest frame carries no lane field; the daemon's card
    // lanes are fingerprinted by their exact choice sets (forge.rs):
    //   memory_review    -> ["Keep", "Forget"]
    //   research_report  -> ["Use", "Discard", "Ask more"]
    //   policy_ask       -> ["Allow this action", "Deny"]
    //   anything else    -> ad-hoc "question" (still actionable if it has
    //                        choices — buttons are harmless and useful)
    fun laneForChoices(choices: List<String>): String {
        val c = choices.map { it.trim().lowercase() }
        return when {
            c == listOf("use", "discard", "ask more") -> RESEARCH
            c == listOf("keep", "forget") -> MEMORY
            c.size == 2 && c[0].startsWith("allow") -> APPROVAL
            else -> QUESTIONS
        }
    }

    /** Which choice indices become notification buttons (max 2). */
    fun actionIndices(choices: List<String>): List<Int> =
        choices.indices.take(2).toList()

    /** Button label: the choice text, clamped to 24 chars. */
    fun actionLabel(choice: String): String =
        choice.trim().takeIf { it.isNotEmpty() }?.let { if (it.length > 24) it.take(21) + "…" else it }
            ?: "OK"

    // ---- avatar cache naming (mirror avatarFileName) ----
    /** `avatars/<first 16 hex chars of sha256(url)>` under cacheDir. */
    fun avatarFileName(url: String): String {
        val d = MessageDigest.getInstance("SHA-256").digest(url.toByteArray())
        return "avatars/" + d.take(8).joinToString("") { "%02x".format(it) }
    }
}
