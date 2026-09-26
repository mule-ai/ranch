/**
 * ranch-tools: pi extension exposing the daemon's agent tools.
 *
 * Registers ranch_spawn / ranch_send / ranch_status / ranch_read /
 * ranch_close as real pi tools. Tool calls go to the ranch daemon's
 * loopback control API (RANCH_CONTROL_TOKEN + RANCH_CONTROL_PORT are
 * exported into this process by ranchd at spawn; RANCH_CONTROL_PANE
 * identifies this agent's pane for the daemon's ownership checks).
 *
 * Every tool call is synchronous: the HTTP request blocks until the
 * daemon answers (for spawns with a callback, that means until the
 * spawned sub-agent finishes its turn or times out).
 *
 * Install: ranchd spawns pi with `-e <ranch-ext-dir>/index.js`
 * (pilocal), or load manually:
 *   pi --mode rpc -e ./ranch-pi-ext/index.js
 *
 * NOTE: the tool functions are async; fetch() keeps the event loop
 * alive, which is what keeps pi's RPC reader fed during the (possibly
 * minutes-long) spawn callback wait.
 */

const PORT = process.env.RANCH_CONTROL_PORT;
const TOKEN = process.env.RANCH_CONTROL_TOKEN;
const PANE = process.env.RANCH_CONTROL_PANE || "";

async function ctl(tool, body) {
	if (!PORT || !TOKEN) {
		return { error: "ranch control API not configured (not spawned by ranchd?)" };
	}
	const res = await fetch(`http://127.0.0.1:${PORT}/agent/${tool}`, {
		method: "POST",
		headers: {
			"content-type": "application/json",
			authorization: `Bearer ${TOKEN}`,
			"x-ranch-pane": PANE,
		},
		body: JSON.stringify(body ?? {}),
	});
	if (!res.ok) {
		const text = await res.text().catch(() => "");
		return { error: `ranchd ${res.status}: ${text.slice(0, 300)}` };
	}
	const frames = await res.json();
	// the daemon returns the reply frames; surface them
	const out = {};
	for (const f of frames) {
		if (f.t === "Error") out.error = f.message;
		else out[f.t] = f;
	}
	return out;
}

function lastRowText(reply) {
	const done = reply.AgentDone;
	if (done?.last_row?.text) return done.last_row.text;
	if (done?.last_row?.tool_name) return `(${done.last_row.tool_name})`;
	return "";
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Wait for a spawned pane to finish its FIRST turn: poll status until
 * working->idle, then read the final row. This is the blocking-callback
 * contract — the tool call doesn't return until the sub-agent answers.
 */
async function awaitCallback(port, pane, timeoutS) {
	const deadline = Date.now() + (timeoutS ? timeoutS * 1000 : 30 * 60 * 1000);
	let wasWorking = false;
	for (;;) {
		if (Date.now() > deadline) return { outcome: "timeout" };
		await sleep(2000);
		const st = await ctlOn(port, "status", { pane });
		if (st.error) return { outcome: "failed", error: st.error };
		const state = st.AgentStatusOk?.state;
		if (state === "closed") return { outcome: "closed" };
		if (state === "working") {
			wasWorking = true;
		} else if (state === "idle" && wasWorking) {
			const rd = await ctlOn(port, "read", { pane, limit: 5 });
			const last = rd.AgentReadOk?.msgs?.filter((m) => m.role === "assistant").pop();
			return { outcome: "completed", last };
		}
		// still "idle" but never saw working: the prompt may not have
		// landed yet — keep polling until it does or the deadline passes
	}
}

// ctl with an explicit port override (awaitCallback runs in the caller
// agent's process — same env, so ctl() suffices; kept for symmetry)
const ctlOn = ctl;

export default function (pi) {
	// tools only make sense when spawned by ranchd (there is no pane id
	// to anchor ownership otherwise)
	if (!PORT || !TOKEN) return;

	pi.registerTool({
		name: "ranch_spawn",
		label: "Ranch Spawn",
		description:
			"Spawn a new AI agent pane in the ranch terminal (visible to the human beside your pane). " +
			"The agent runs with an initial prompt. Returns {spawn_id, session, pane}. " +
			"With callback=true (default), the tool BLOCKS until the spawned agent finishes its turn " +
			"and returns its result — use it like a subagent call. kind: \"pi\" (local, fast) or \"forge\" (durable). " +
			"mode: \"split\" (pane next to you) or \"session\" (own session).",
		promptGuidelines: [
			"Use ranch_spawn to delegate self-contained subtasks (research, tests, a refactor) to a subagent.",
			"The human sees the spawned pane live and can watch or interject; close it with ranch_close when done.",
		],
		parameters: {
			type: "object",
			properties: {
				kind: { type: "string", enum: ["pi", "forge"], description: "agent runtime (default pi)" },
				prompt: { type: "string", description: "the initial task for the spawned agent" },
				name: { type: "string", description: "optional session name (mode=session)" },
				cwd: { type: "string", description: "optional working directory (default: your cwd)" },
				mode: { type: "string", enum: ["split", "session"], description: "split = pane beside you" },
				callback: { type: "boolean", description: "block until the sub-agent finishes (default true)" },
				timeout_s: { type: "number", description: "max seconds to wait on the callback" },
			},
			required: ["prompt"],
		},
		async execute(_id, params) {
			const callback = params.callback !== false;
			const reply = await ctl("spawn", {
				kind: params.kind || "pi",
				prompt: params.prompt,
				name: params.name,
				cwd: params.cwd,
				mode: params.mode || "split",
				callback: false, // ack immediately; blocking is done here
			});
			if (reply.error) {
				return { content: [{ type: "text", text: `ranch_spawn failed: ${reply.error}` }] };
			}
			const ok = reply.AgentSpawnOk;
			if (!ok) return { content: [{ type: "text", text: "ranch_spawn: unexpected reply" }] };
			if (!callback) {
				return {
					content: [{
						type: "text",
						text: `spawned pane ${ok.pane} in session ${ok.session} (spawn ${ok.spawn_id}); use ranch_send/ranch_read/ranch_close to manage it`,
					}],
				};
			}
			// callback=true: poll until the sub-agent finishes its first
			// turn, then return its result as the tool result
			const done = await awaitCallback(PORT, ok.pane, params.timeout_s);
			if (done.outcome === "completed") {
				return {
					content: [{
						type: "text",
						text: `sub-agent ${ok.pane} finished: ${done.last?.text?.trim() || "(no text output)"}`,
					}],
				};
			}
			return {
				content: [{
					type: "text",
					text: `sub-agent ${ok.pane} ${done.outcome}${done.error ? `: ${done.error}` : ""} — pane left open; inspect with ranch_status/ranch_read or close with ranch_close`,
				}],
			};
		},
	});

	pi.registerTool({
		name: "ranch_send",
		label: "Ranch Send",
		description:
			"Send a message to an agent in another ranch pane (one you spawned). " +
			"delivery=\"steer\" redirects the agent as soon as possible; \"queue\" delivers after its current turn.",
		parameters: {
			type: "object",
			properties: {
				pane: { type: "string", description: "target pane id (from ranch_spawn)" },
				text: { type: "string", description: "the message" },
				delivery: { type: "string", enum: ["steer", "queue"] },
			},
			required: ["pane", "text"],
		},
		async execute(_id, params) {
			const reply = await ctl("send", params);
			if (reply.error) return { content: [{ type: "text", text: `ranch_send failed: ${reply.error}` }] };
			return { content: [{ type: "text", text: `sent to ${params.pane}` }] };
		},
	});

	pi.registerTool({
		name: "ranch_status",
		label: "Ranch Status",
		description: "Check an agent pane's state (working/idle) and model. Cheap; use before ranch_read.",
		parameters: {
			type: "object",
			properties: { pane: { type: "string" } },
			required: ["pane"],
		},
		async execute(_id, params) {
			const reply = await ctl("status", params);
			if (reply.error) return { content: [{ type: "text", text: `ranch_status failed: ${reply.error}` }] };
			const ok = reply.AgentStatusOk;
			return {
				content: [{
					type: "text",
					text: `pane ${params.pane}: ${ok?.state ?? "unknown"}${ok?.model ? ` (${ok.model})` : ""}`,
				}],
			};
		},
	});

	pi.registerTool({
		name: "ranch_read",
		label: "Ranch Read",
		description: "Read recent conversation rows from an agent pane you spawned (newest last).",
		parameters: {
			type: "object",
			properties: {
				pane: { type: "string" },
				since_seq: { type: "number", description: "only rows after this seq" },
				limit: { type: "number", description: "max rows (default 50)" },
			},
			required: ["pane"],
		},
		async execute(_id, params) {
			const reply = await ctl("read", params);
			if (reply.error) return { content: [{ type: "text", text: `ranch_read failed: ${reply.error}` }] };
			const ok = reply.AgentReadOk;
			if (!ok?.msgs?.length) return { content: [{ type: "text", text: "(no rows)" }] };
			const text = ok.msgs
				.map((m) => `[${m.seq}] ${m.role}${m.tool_name ? `(${m.tool_name})` : ""}: ${m.text}`)
				.join("\n");
			return { content: [{ type: "text", text }] };
		},
	});

	pi.registerTool({
		name: "ranch_close",
		label: "Ranch Close",
		description:
			"Close an agent pane you spawned when its work is done (the human can also close it). " +
			"Returns the pane's final output summary.",
		parameters: {
			type: "object",
			properties: { pane: { type: "string" } },
			required: ["pane"],
		},
		async execute(_id, params) {
			const reply = await ctl("close", params);
			if (reply.error) return { content: [{ type: "text", text: `ranch_close failed: ${reply.error}` }] };
			const done = reply.AgentDone;
			if (done) {
				return { content: [{ type: "text", text: `closed ${params.pane} (${done.outcome}): ${lastRowText(reply) || "(no output)"}` }] };
			}
			return { content: [{ type: "text", text: `closed ${params.pane}` }] };
		},
	});

	pi.registerTool({
		name: "ranch_ask",
		label: "Ranch Ask",
		description:
			"Ask the human user a question and block until they answer — there is no timeout; " +
			"the user may take hours. " +
			"The question is shown live on every attached ranch interface (TUI, web, phone) and can trigger a " +
			"phone notification. Provide 2-5 short choices, mark one as suggested, and keep free_text=true so the " +
			"user can type a custom answer. Use multi=true when more than one choice may apply. " +
			"Returns the user's answer (selected choices and/or free text). If the daemon restarts while blocked, " +
			"returns 'no answer (ask lost)'.",
		promptGuidelines: [
			"Use ranch_ask when you genuinely need a human decision (ambiguous requirements, destructive actions, preferences).",
			"Choices must be short (a few words each); put nuance in the question text.",
			"Prefer multi=false unless the question really allows several options at once.",
		],
		parameters: {
			type: "object",
			properties: {
				question: { type: "string", description: "the question to ask" },
				choices: { type: "array", items: { type: "string" }, description: "2-5 short answer options" },
				suggested: { type: "number", description: "0-based index of the suggested choice" },
				multi: { type: "boolean", description: "true = user may select multiple choices (default false)" },
				free_text: { type: "boolean", description: "also allow a blank user-typed answer (default true)" },
			},
			required: ["question"],
		},
		async execute(_id, params) {
			const reply = await ctl("ask", {
				question: params.question,
				choices: params.choices || [],
				suggested: params.suggested,
				multi: !!params.multi,
				free_text: params.free_text !== false,
			});
			if (reply.error) {
				return { content: [{ type: "text", text: `ranch_ask failed: ${reply.error}` }] };
			}
			const ok = reply.AgentAskOk;
			if (!ok || !ok.ask_id) {
				return { content: [{ type: "text", text: "ranch_ask: no ask_id in reply" }] };
			}
			// block (in the tool) until the user answers — no timeout:
			// the user may take arbitrarily long. If the daemon restarted,
			// the registry lost the ask and the poll reports "unknown".
			for (;;) {
				await sleep(3000);
				const st = await ctl("ask-status", { ask_id: ok.ask_id });
				if (st.error) continue;
				const so = st.AgentAskStatusOk;
				if (!so) continue;
				if (so.state === "unknown") {
					return { content: [{ type: "text", text: "no answer (ask lost — daemon restarted?)" }] };
				}
				if (so.answered) {
					const parts = [];
					if (so.choices && so.choices.length) parts.push("selected: " + so.choices.join(", "));
					if (so.text) parts.push("text: " + so.text);
					return { content: [{ type: "text", text: parts.length ? parts.join(" | ") : "(empty answer)" }] };
				}
			}
		},
	});
}
