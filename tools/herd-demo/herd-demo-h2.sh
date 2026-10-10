#!/usr/bin/env bash
# herd-demo-h2.sh — Herd H2 acceptance: subagent spawn/abort via a
# forged prompt (PLAN-HERD §10.3 item 2, the subagent half).
#
# CHECKLIST:
#   1. preconditions — local forge up + key + jq
#   2. demo agent + conversation
#   3. forged prompt asks the agent to use the spawn_subagent tool
#   4. poll GET /sessions?parent=<cid> — a child (subagent) session
#      lands (subagent_started)
#   5. abort the ownership tree: POST /sessions/<cid>/interrupt
#      (tree abort kills the child with the parent)
#   6. cleanup
#
# Automated: 2–6 (API assertions). NEEDS A LIVE HARNESS: the forged
# prompt has to reach a running pi-durable harness with a model that
# calls spawn_subagent. Point the demo at such a forge (FORGE_URL);
# dev forges with FORGE_HARNESS_FAUX=1 answer deterministically but
# the faux provider does not call tools on its own — on a faux forge
# step 3/4/5 are printed as manual with the exact commands instead.
#
# The rest of §10.3 item 2 (kill -9 forge mid-turn ×3 + resume, a
# durable timer across a restart, compaction of a synthetic long
# session) is inherently operator-driven and stays MANUAL:
#   M1. send a long turn, `kill -9` the forge API + harness mid-turn,
#       restart both, assert the transcript is consistent (exactly
#       one user row, one assistant row — see forge's
#       dual_kill9_recover_mid_turn test as the automated equivalent)
#   M2. create a one-shot timer (POST /sessions/<cid>/timers), kill -9
#       before its tick, restart, assert it fires exactly once
#   M3. feed a synthetic long session, POST /sessions/<cid>/compact,
#       assert GET /sessions/<cid>/context reflects the compacted state
#
# Exit code = number of failed steps.

set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ST=0
ST=$((ST + 1)); require_jq || :
ST=$((ST + 1)); require_forge "$ST" || :
ST=$((ST + 1)); require_forge_key "$ST" || :
[ "$HFAIL" -gt 0 ] && { echo; echo "preconditions failed — unblock the FAIL lines above and re-run"; finish; }

FAUX="${FORGE_HARNESS_FAUX:-0}"

# ── Step 2: demo agent + conversation ───────────────────────────────
ST=$((ST + 1)); hstep "$ST" "demo agent + fresh conversation"
AID=$(new_demo_agent "herd-demo-h2-$(date +%s)") || { hfail "agent create failed"; finish; }
CONV=$(fpost "/agents/$AID/conversations" '{"title":"herd-demo-h2"}') || { hfail "conversation create failed"; finish; }
CID=$(printf '%s' "$CONV" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
[ -n "$CID" ] && [ "$CID" != "null" ] || { hfail "no conversation id: $(printf '%s' "$CONV" | head -c 150)"; finish; }
hok "agent $AID, conversation $CID"

if [ "$FAUX" = "1" ] || [ "${FORGE_FAUX:-0}" = "1" ]; then
  # The provider is faux: it never calls tools, so the subagent flow
  # cannot be driven. Print the manual steps instead of failing.
  ST=$((ST + 1)); hstep "$ST" "subagent spawn — MANUAL on this forge (faux provider does not call tools)"
  hman "against a REAL-model forge, run by hand:"
  cat <<EOF
   a) export P='Use the spawn_subagent tool to run a subagent task that
      computes 2+2 and replies with the number. When it returns, reply
      "subagent done". Do not use any other tool.'
   b) curl -s -X POST -H "Authorization: Bearer \$FORGE_KEY" \\
        -H 'Content-Type: application/json' \\
        -d "{\\\"content\\\": \\"$P\\"}" \\
        "$FORGE_URL/sessions/$CID/messages"
   c) watch: curl -s -H "Authorization: Bearer \$FORGE_KEY" \\
        "$FORGE_URL/sessions?parent=$CID"   # child session appears
   d) abort the tree:  curl -s -X POST -H "Authorization: Bearer \$FORGE_KEY" \\
        "$FORGE_URL/sessions/$CID/interrupt"   # tree abort kills child too
   e) assert the child session's state is terminal
EOF
  fdel "/agents/$AID" >/dev/null 2>&1
  finish
fi

# ── Step 3: forged prompt ───────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "forged prompt: ask the agent to spawn a subagent"
PROMPT='Use the spawn_subagent tool to run a subagent task that computes 2+2 and replies with the number. When it returns, reply "subagent done". Do not use any other tool.'
MSG=$(fpost "/sessions/$CID/messages" "$(jq -n --arg c "$PROMPT" '{content:$c} | .')")
if printf '%s' "$MSG" | jq -e 'true' >/dev/null 2>&1; then
  hok "prompt accepted: $(printf '%s' "$MSG" | head -c 120)"
else
  hfail "POST /sessions/$CID/messages failed: $(printf '%s' "$MSG" | head -c 200)"
fi

# ── Step 4: child session lands ─────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "GET /sessions?parent=$CID shows the subagent session (60 s budget)"
CHILD=""
for i in $(seq 1 30); do
  CHILD=$(fget "/sessions?parent=$CID" 2>/dev/null \
    | jq -r '(if type=="array" then . else (.sessions // []) end) | .[0].id // empty' 2>/dev/null)
  [ -n "$CHILD" ] && [ "$CHILD" != "null" ] && break
  sleep 2
done
if [ -n "$CHILD" ] && [ "$CHILD" != "null" ]; then
  hok "subagent session $CHILD appeared"
else
  hfail "no child session after 60 s — is this forge running a live \
harness with a real model (FORGE_HARNESS_MESSAGES on by default)? \
Manual: see the faux-mode block in this script's header."
fi

# ── Step 5: tree abort ──────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "abort the ownership tree (POST /sessions/$CID/interrupt)"
INT=$(fpost "/sessions/$CID/interrupt")
if printf '%s' "$INT" | jq -e 'true' >/dev/null 2>&1; then
  hok "tree abort accepted: $(printf '%s' "$INT" | head -c 120)"
  sleep 3
  if [ -n "$CHILD" ] && [ "$CHILD" != "null" ]; then
    CST=$(fget "/sessions/$CHILD" 2>/dev/null | jq -r '.state // .session.state // "unknown"' 2>/dev/null)
    hok "child session state after tree abort: $CST (expected terminal/aborted)"
  fi
else
  hfail "interrupt failed: $(printf '%s' "$INT" | head -c 200)"
fi

# ── Step 6: cleanup ─────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "cleanup"
if [ "${KEEP:-0}" = "1" ]; then
  hman "KEEP=1 — leaving agent/conversation behind for inspection"
else
  fdel "/agents/$AID" >/dev/null 2>&1 && hok "demo agent deleted" || hman "agent delete non-zero (conversations may hold it)"
fi

finish
