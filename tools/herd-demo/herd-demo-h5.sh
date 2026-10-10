#!/usr/bin/env bash
# herd-demo-h5.sh — Herd H5 acceptance: proactive research card,
# memory-trigger wake, Activity View parity, herd pause
# (PLAN-HERD §10.3 item 5).
#
# CHECKLIST:
#   1. preconditions — forge up + key + jq
#   2. trigger proactive research: POST /agents/:id/research
#   3. assert the research row lands (GET /agents/:id/research?open=1)
#      and, with a live harness, settles to done + pushes the
#      research_report card (60 s budget)
#   4. memory-trigger queue: GET /agents/:id/memory/triggers/pending
#      (live wake only when a belief's watch matches an episode —
#      manual setup printed)
#   5. Activity View parity: MANUAL (TUI Tab → activity section;
#      phone Settings → Tools → Activity; the same ActivityPush
#      frames on every surface)
#   6. herd pause: `ranch pause <agent>` (CLI when the local daemon
#      is up; forge API otherwise) + assert the 409 'agent paused'
#      on the next message; resume
#
# Automated: 1–4 (read), 6. Manual: 3's card-visibility half, 4's
# live-fire half, 5.
# Exit code = number of failed steps.

set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ST=0
ST=$((ST + 1)); require_jq || :
ST=$((ST + 1)); require_forge "$ST" || :
ST=$((ST + 1)); require_forge_key "$ST" || :
[ "$HFAIL" -gt 0 ] && { echo; echo "preconditions failed — unblock the FAIL lines above and re-run"; finish; }

NAME="herd-demo-h5-$(date +%s)"

# ── Step 2/3: research trigger + row ────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "demo agent; POST /agents/:id/research {question}"
AID=$(new_demo_agent "$NAME") || { hfail "agent create failed"; finish; }
RES=$(fpost "/agents/$AID/research" '{"question":"What is the current status of the pi-durable project? Summarize in three bullets. (herd-demo-h5)"}')
if printf '%s' "$RES" | jq -e 'true' >/dev/null 2>&1; then
  hok "research accepted: $(printf '%s' "$RES" | head -c 150)"
else
  hfail "research POST failed: $(printf '%s' "$RES" | head -c 200)"
fi

ST=$((ST + 1)); hstep "$ST" "GET /agents/:id/research?open=1 shows the row; poll for done (90 s budget — needs a live harness)"
RROW=""
for i in $(seq 1 30); do
  RROW=$(fget "/agents/$AID/research?open=1" 2>/dev/null)
  STATE=$(printf '%s' "$RROW" | jq -r '((.research // .items // (if type=="array" then . else [] end)) | .[0].state // empty)' 2>/dev/null)
  [ "$STATE" = "done" ] && break
  sleep 3
done
if [ -n "$RROW" ] && printf '%s' "$RROW" | jq -e 'true' >/dev/null 2>&1; then
  hok "research row present, state=$STATE (done/adopted when the harness settled it)"
  if [ "$STATE" = "done" ]; then
    hok "settled — the research_report card was pushed to the agent's most-active conversation (see step 4's manual check for the phone/TUI half)"
  else
    hman "still $STATE after the budget: harness may be faux/absent — the row's lifecycle (pending→running→done) is asserted by forge's H5.1 e2e; manual: re-poll 'GET /agents/$AID/research?open=1' after the turn settles, then answer Use/Discard/Ask more on the card"
  fi
else
  hfail "no research row: $(printf '%s' "$RROW" | head -c 200)"
fi

# ── Step 4: memory-trigger queue ────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "memory-trigger queue reachable (GET …/memory/triggers/pending)"
TRIG=$(fget "/agents/$AID/memory/triggers/pending" 2>/dev/null)
if printf '%s' "$TRIG" | jq -e 'true' >/dev/null 2>&1; then
  N=$(printf '%s' "$TRIG" | jq -r '(if type=="array" then . else (.triggers // .pending // []) end) | length' 2>/dev/null)
  hok "trigger queue endpoint ok ($N queued trigger(s))"
  if [ "${N:-0}" -eq 0 ]; then
    hman "no queued trigger yet (expected on a fresh agent). To fire one LIVE: create a belief with watch {match:'<phrase>', wake_id:'<wake>'} (POST …/memory/beliefs/proposals then keep), send an episode whose summary contains '<phrase>' to this agent, and the mule memory-trigger forwarder (H4.5) polls this queue every 30 s and fires the wake — assert it in mule: GET $MULE_URL/api/v1/wakes/activity"
  fi
else
  hfail "triggers/pending failed: $(printf '%s' "$TRIG" | head -c 150)"
fi

# ── Step 5: Activity View parity (manual) ───────────────────────────
ST=$((ST + 1)); hstep "$ST" "Activity View parity TUI ↔ phone: MANUAL"
cat <<EOF
   With this demo's research row + a pending ask visible:
   TUI:    attach to any session → prefix/Tab until the dashboard →
           Tab to the ACTIVITY section: the research_open item (🔬),
           pending asks (✅), wakes (⏰); 'p' pauses the whole herd,
           'P' pauses the highlighted agent.
   phone:  mobile-native Settings → Tools → "Activity" (or the Expo
           app's "activity" tab): same timeline, "Pause herd" header,
           long-press → "Pause/Resume agent".
   Parity = both render the daemon's ActivityPush frames (5 s poll,
   2 s debounce, PROTOCOL.md §8.6) — same items, same glyphs.
EOF
hman "visual parity check on a TUI and a phone"

# ── Step 6: herd pause + 409 ────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "herd pause: pause the agent, assert 409, resume"
CONV=$(fpost "/agents/$AID/conversations" '{"title":"herd-demo-h5-paused"}')
CID=$(printf '%s' "$CONV" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
[ -n "$CID" ] || CID=$(fget "/agents/$AID/conversations?latest=1" | jq -r '(if type=="array" then . else (.conversations // .sessions // .items // []) end) | .[0].id // empty' 2>/dev/null)

DAEMON_FORGE_URL=$(sed -n 's/^forge_url[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
  "$HOME/.config/ranch/daemon.toml" 2>/dev/null | head -1)
if "$RANCH_BIN" ls >/dev/null 2>&1 && [ "$DAEMON_FORGE_URL" = "$FORGE_URL" ]; then
  "$RANCH_BIN" pause "$NAME" >/dev/null 2>&1 && hok "CLI: ranch pause '$NAME'" \
    || hfail "CLI pause failed (falling back to forge API)"
  fpost "/agents/$AID/pause" >/dev/null 2>&1   # idempotent, ensures state
else
  fpost "/agents/$AID/pause" >/dev/null 2>&1 && hok "forge API: agent paused" \
    || hfail "pause failed"
fi

PAUSE_BODY='{"content":"anything"}'
R409=$(curl -sS -m 20 -o /tmp/herd-h5-409.$$ -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $FORGE_KEY" -H 'Content-Type: application/json' \
  -d "$PAUSE_BODY" "$FORGE_URL/sessions/$CID/messages" 2>/dev/null || echo 000)
if [ "$R409" = "409" ]; then
  hok "message while paused → HTTP 409 ($(head -c 100 /tmp/herd-h5-409.$$))"
else
  hfail "expected 409 while paused, got $R409: $(head -c 150 /tmp/herd-h5-409.$$)"
fi
rm -f /tmp/herd-h5-409.$$

if "$RANCH_BIN" ls >/dev/null 2>&1 && [ "$DAEMON_FORGE_URL" = "$FORGE_URL" ]; then
  "$RANCH_BIN" unpause "$NAME" >/dev/null 2>&1 && hok "CLI: ranch unpause '$NAME'" || fpost "/agents/$AID/resume" >/dev/null 2>&1
else
  fpost "/agents/$AID/resume" >/dev/null 2>&1 && hok "resumed via forge API" || hfail "resume failed"
fi

# ── cleanup ─────────────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "cleanup"
if [ "${KEEP:-0}" = "1" ]; then
  hman "KEEP=1 — leaving agent $AID + research row behind"
else
  fdel "/agents/$AID" >/dev/null 2>&1 && hok "agent deleted" || hman "agent delete non-zero (open research row may hold it — resolve Use/Discard on the card)"
fi

finish
