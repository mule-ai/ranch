# Verifying Herd — a user's guide

Everything in the Herd plan (agents, memory, proactivity, policy, the
voice loop) is built and committed. This page is how you, as the human,
verify it end to end. No jargon: each section is something you *do* and
something you *see*.

## Where things live (so the flows make sense)

- **Your laptop** — `ranchd` (the terminal/dashboard/`ranch` commands) and
  the phone app talk to it. This box runs nothing else.
- **The lab** (mini + the lab network): the agents' actual home.
  - *forge* — where the agents live and think (has the new memory + voice
    support).
  - *mule* — where agent behavior is scripted: scheduled wakes, the
    reflection (learning) workflow, policy checks, and the voice-call
    plumbing. New machine, on port 8091, LAN-only.
  - *Parakeet / Kokoro* — the speech recognition and speech synthesis,
    running on the lab's GPU box.
- **Your phone** — the Android app (the Kotlin one, `dev.ranch.android`).

## One-time setup (mostly done — check the boxes you haven't)

Done on 2026-10-10/11 (no action needed):

- [x] **Lab machines up.** `ssh -tt mini 'sudo lab-alc-reconcile'` has
  been run six times; the forge machine carries the Herd harness +
  pgvector + the restored GPU voice backends, and the mule machine is
  live at port 8091 with its workflows/wakes/modules restored. (If a
  future image rebuild needs it: `scp` the changed `ct/forge`/`ct/mule`
  files to `mini:/Users/jbutler/alcbld/repo/...`, build the image in
  the machine, swap `/var/lib/lab-alc/images/*-alc-latest.tar`, then
  reconcile. Expect brief agent downtime.)
- [x] **Mule secrets saved** to the lab Vaultwarden (`vault.butler.ooo`)
  in the `mule-private` folder: the admin API key (`sk_mule_…`) and the
  encryption secret.
- [x] **The "billing" specialist agent exists** in lab forge
  (restricted tools, read-only, org memory scope) and the
  `billing-daily` mule workflow + 07:00 UTC wake is registered —
  so Flow 1 below can start from "talk to it" if you want.

Still yours to do (phone):

1. **Install the new phone app.** The current release build is at
   `mobile-native/app/build/outputs/apk/release/app-release.apk` on the
   laptop — sideload it (or `adb install`). It's the same app, updated
   with the agent screens, activity feed, calls, and notifications.
2. **Two small settings in the phone app** (Settings screen):
   - *Voice input* section: forge address `http://10.10.199.29:8110` and
     the lab forge key (the one your laptop uses — it's in
     `~/.config/ranch/daemon.toml`).
   - *Voice call* section: mule address `http://10.10.199.29:8091` and
     the mule key from the `mule-private` vault folder.

## Flow 1 — Create your first specialist agent

*(The `billing` agent from the plan already exists in lab forge —
created during setup, with the `billing-daily` wake registered. To
verify *creation* itself, make a second one (e.g. `travel`), or just
skip to Flow 2 and confirm `billing` appears on both surfaces.)*

The "billing" agent from the plan: it gets a restricted tool set,
org-shared memory, and wakes up every morning at 7:00 on its own.

**Phone:** open the app → Agents screen → the **+** button. Fill in:
name `billing`, memory scope `org` (org label: whatever your other
agents use — or `butler`), a daily wake at `07:00`. Save.

**Terminal** (same thing, more knobs): `ranch agents new` — it walks you
through name → avatar (pick an image file) → model profile → which tools
it may use (check/uncheck — for billing, uncheck *bash* and *edit* so it
can reason but not touch files) → memory scope → home machine → default
wakes.

**You should see:** the agent appear in the phone's Agents list *and* in
the terminal dashboard (the Agents section). It has a name, an avatar
circle, and — after its first message — it's tappable.

*Success:* both surfaces list `billing`, and the terminal prints a
confirmation that its morning wake was registered.

## Flow 2 — Talk to it from anywhere

**Phone:** tap `billing` → you get a chat pane. Type "What should I watch
for in this month's bills?" and send. A small dot shows it's thinking;
the answer arrives as a chat message.

**Terminal:** `ranch agent billing "Summarize our recurring charges"` —
the reply prints under your message. `ranch agent billing --new
"Start a fresh conversation"` opens a new thread; the phone's
"new conversation" gesture does the same. There's also a *fork*: from a
conversation you can branch off ("what if we also considered plan B")
without disturbing the original.

*Success:* the same agent, same memory, reachable from both places, and
a forked conversation that leaves the original intact.

## Flow 3 — Call it (the voice loop)

This is the showcase: a two-way voice conversation where the agent
speaks back, and you can interrupt it mid-sentence.

**Phone:** Agents screen → the **📞** phone icon on an agent's row. The
call screen comes up with a status line: *listening* (your mic is open)
→ *thinking* (the agent is working) → *speaking* (it's talking).

**Terminal:** `ranch call billing` — same call. If this laptop has no
microphone (it doesn't), it falls back to typing: each line you type is
your "voice", and you read its answers on screen. Type `bye` to hang up.

**Try the interruption:** let it start speaking, then talk over it.
Within a fraction of a second it stops talking and starts listening to
you — that's the barge-in.

*Success:* a real back-and-forth conversation where speech in becomes
understood words and its reply comes back as speech; talking over it
silences it immediately; hanging up (button or `bye` or Ctrl-C) ends
the call cleanly and nothing looks broken afterwards.

## Flow 4 — Dictate into chat (voice input)

**Phone:** in any agent's chat pane, next to the send button there's a
🎤. Hold it, speak, release. Your words appear in the text box as text
(you still press send). It uses the lab's speech recognition, so it
works for real speech; in airplane mode the phone's own recognizer is
the fallback.

*Success:* your dictation lands in the message field legibly, and the
round trip takes well under a second of perceived delay.

## Flow 5 — See what the herd is doing in the background

Agents do work you didn't ask for at that moment (scheduled tasks,
reflection, research). Both surfaces have an **Activity View** that shows
it as it happens.

**Terminal:** open the dashboard (plain `ranch`), then cycle the section
tabs (the Tab key or `o`): *sessions → agents → activity*. The activity
view is a timeline grouped by agent: which agent did what, when, and
whether it's still going.

**Phone:** the Activity tab shows the same timeline.

**Deep-link:** pick an entry and it opens that conversation; pick an
agent's group and you can pause just that agent (phone: long-press;
terminal: `P` on the highlighted agent). `p` in the activity view pauses
or resumes the **whole herd**.

*Success:* without asking any agent anything, you can watch the herd's
background work arrive live, on both surfaces, and stop it or one agent
of it with one key/tap.

## Flow 6 — It wakes up on its own (proactivity)

The `billing` agent's daily 07:00 wake: the next morning, without you
sending anything, it should have run a turn in its conversation.

**Check:** open `billing`'s conversation (phone or terminal) — there's a
morning message you didn't type (it's tagged with the wake that fired
it). The Activity View also has the entry.

*Success:* an unattended turn appears on schedule, attributed to the
wake, and the agent's day starts from its own initiative.

## Flow 7 — Approvals on a locked phone

When an agent does something that should need your say-so, you get a
notification card — and it works **while the phone is locked**.

To make one happen deliberately: ask an agent for something that triggers
a review (the memory system generates "belief" suggestions that always
come to you as a card: *Keep / Forget*). Tap the notification on the
lock screen, read the suggested belief, and tap **Keep** (or Forget)
right there — the agent's memory updates without you unlocking the
phone.

The same mechanism covers tool-policy approvals (*Allow this action /
Deny*) and research reports (*Use / Discard / Ask more*): the card shows
up as a notification with the buttons, and tapping one sends your answer
straight to the agent.

*Success:* a locked phone answers the agent's question; the agent's
behavior reflects your choice (a kept belief shows up in what it knows;
a used research report becomes part of the conversation).

## Flow 8 — Ask it to research something

**Phone or terminal:** "research the pros and cons of our three hosting
options". The agent goes to work *read-only* — it can read files, search,
and fetch web pages, but it structurally cannot write or run commands
(that's enforced, not a promise). When it's done, a **research report**
card appears: a summary with the buttons *Use / Discard / Ask more*.

*Success:* the report card arrives as a notification + in the
conversation; **Use** folds it into the conversation; **Ask more** sends
a follow-up; **Discard** closes it out.

## If something looks wrong (quick checks)

| Symptom | First check |
|---|---|
| No agents on the phone | Is the laptop's `ranchd` running (`pgrep -f ranchd`)? Is forge up? |
| Call says "mule unreachable" | Is the mule machine up on mini? `curl http://10.10.199.29:8091/health` from the laptop. |
| Voice input says STT disabled | Phone Settings: forge address/key set? `curl http://10.10.199.29:8110/v1/audio/voices` should now say STT + TTS enabled (it didn't before the rebuild). |
| Agent never answers | Is forge up? `curl http://10.10.199.29:8110/health`. Right after a machine rebuild, give it a few minutes (first-boot restore). If the answer is "harness unavailable", the `forge-harness` service is down inside the forge machine — it starts with the machine; check it there (`container machine run -n forge -- systemctl status forge-harness forge-api`). |
| Wakes don't fire | Mule up + laptop on the lab network; the daemon's activity view should show wake entries once one fires. |
| Notifications silent for one agent | Settings → per-agent notifications: that agent may be muted or on *quiet*. |

## What was deliberately left for later (so it's not a surprise)

- The "stop this task" button on task notifications (the stop/abort
  plumbing is a later slice; pause exists now).
- Action buttons on notifications in the *other* (web-preview) app —
  the Kotlin app is the reference and has them all.
- The full agent-creation wizard on the phone (the phone form creates a
  baseline agent; the terminal wizard has the full tool allowlist +
  avatar).
- "Pause the agent while you sign in to a website for it" — the
  sign-in broker works via the approval card today; the pause variant
  is the follow-up.
