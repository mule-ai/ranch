#!/usr/bin/env bash
# herd-demo-h1.sh — Herd H1 acceptance: agent CRUD + talk/continue/fork
# from all surfaces (PLAN-HERD §10.3 item 1).
#
# CHECKLIST (numbered steps below):
#   1. preconditions — local forge up + key + jq
#   2. create agent via POST /agents (CRUD: C)
#   3. registry row lands — GET /agents contains it
#   4. CLI talk: `ranch agent <name> hi` (automated when the local
#      daemon is configured against THIS forge; otherwise the step
#      prints the manual command — it is a surface, not a data check)
#   5. continue — GET /agents/:id/conversations?latest=1 + first
#      message lands as a row (POST …/conversations/:cid/messages)
#   6. fork — POST /agents/:id/conversations {fork_from} copies the
#      history into a sibling conversation
#   7. cleanup (DELETE /agents/:id; KEEP=1 to inspect)
#
# Automated: 2, 3, 5, 6, 7 (pure API assertions).
# Manual/conditional: 4 (needs the local ranchd pointed at this forge).
# Exit code = number of failed steps.

set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ST=0
require_jq || :
ST=$((ST + 1)); require_forge "$ST" || :
ST=$((ST + 1)); require_forge_key "$ST" || :
[ "$HFAIL" -gt 0 ] && { echo; echo "preconditions failed — unblock the FAIL lines above and re-run"; finish; }

NAME="herd-demo-h1-$(date +%s)"

# ── Step 2: create agent ────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "POST /agents creates '$NAME'"
AID=$(new_demo_agent "$NAME") || { hfail "agent create failed"; finish; }
hok "agent id=$AID"

# ── Step 3: registry row ────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "GET /agents lists the new agent"
LIST=$(fget "/agents") || { hfail "GET /agents failed"; finish; }
if printf '%s' "$LIST" | jq -e --arg id "$AID" \
    '((.agents // . // [])[] | select(.id==$id or .agent_id==$id))' >/dev/null 2>&1; then
  hok "agent present in registry"
else
  hfail "agent $AID not in GET /agents: $(printf '%s' "$LIST" | head -c 200)"
fi

DAEMON_FORGE_URL=$(sed -n 's/^forge_url[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
  "$HOME/.config/ranch/daemon.toml" 2>/dev/null | head -1)

# ── Step 4: CLI talk ────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "CLI surface: \`ranch agent $NAME hi\`"
if [ "$DAEMON_FORGE_URL" = "$FORGE_URL" ]; then
  OUT=$("$RANCH_BIN" agent "$NAME" "hi" 2>&1) && \
    hok "ranch agent '$NAME' hi ran: $(printf '%s' "$OUT" | head -c 120)" || \
    hfail "ranch agent '$NAME' hi failed: $(printf '%s' "$OUT" | head -c 200)"
else
  hman "local ranchd is configured against $DAEMON_FORGE_URL, not \
$FORGE_URL — run by hand where the daemon points here: \
\$ $RANCH_BIN agent $NAME hi        # opens the agent's most-active conversation"
fi

# ── Step 5: continue + first message row ────────────────────────────
ST=$((ST + 1)); hstep "$ST" "continue: open most-active conversation, send 'hi', assert the user row lands"
CONV=$(fget "/agents/$AID/conversations?latest=1") || { hfail "GET conversations failed"; finish; }
CID=$(printf '%s' "$CONV" | jq -r '(if type=="array" then . else (.conversations // .sessions // .items // []) end) | .[0].id // empty' 2>/dev/null)
if [ -z "$CID" ]; then
  CONV2=$(fpost "/agents/$AID/conversations" '{"title":"herd-demo-h1"}')
  CID=$(printf '%s' "$CONV2" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
fi
if [ -z "$CID" ]; then
  hfail "no conversation (created/none): $(printf '%s' "$CONV" | head -c 200)"
else
  MSG=$(fpost "/agents/$AID/conversations/$CID/messages" '{"content":"hi"}')
  CODE=$?
  sleep 2
  ROWS=$(fget "/sessions/$CID/messages" 2>/dev/null)
  if printf '%s' "$ROWS" | jq -e '.[] | select((.content // .text // "") | contains("hi"))' >/dev/null 2>&1; then
    hok "conversation $CID exists and the 'hi' user row is visible (POST rc=$CODE)"
  else
    hfail "sent 'hi' (POST rc=$CODE, resp: $(printf '%s' "$MSG" | head -c 150)) but no matching row in GET /sessions/$CID/messages"
  fi
fi

# ── Step 6: fork ────────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "fork: sibling conversation copies the history"
FORK=$(fpost "/agents/$AID/conversations" "$(jq -n --arg from "$CID" '{fork_from:$from}')") || { hfail "fork POST failed"; finish; }
FID=$(printf '%s' "$FORK" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
if [ -n "$FID" ] && [ "$FID" != "null" ]; then
  FROWS=$(fget "/sessions/$FID/messages" 2>/dev/null)
  if printf '%s' "$FROWS" | jq -e '.[] | select((.content // .text // "") | contains("hi"))' >/dev/null 2>&1; then
    hok "fork $FID created and the 'hi' row was copied into it"
  else
    hfail "fork $FID created but its message list is empty: $(printf '%s' "$FROWS" | head -c 150)"
  fi
else
  hfail "fork returned no conversation id: $(printf '%s' "$FORK" | head -c 200)"
fi

# ── Step 7: cleanup (CRUD: D) ───────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "cleanup: DELETE /agents/$AID"
if [ "${KEEP:-0}" = "1" ]; then
  hman "KEEP=1 — leaving agent $AID ($NAME) behind for inspection"
else
  fdel "/agents/$AID" >/dev/null 2>&1 && hok "agent deleted" || \
    hman "delete returned non-zero (agent may keep conversations until they close) — inspect: GET /agents"
fi

finish
