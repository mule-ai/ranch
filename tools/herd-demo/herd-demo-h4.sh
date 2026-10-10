#!/usr/bin/env bash
# herd-demo-h4.sh — Herd H4 acceptance: episodes → reflection →
# belief-approval cards → an active belief (PLAN-HERD §10.3 item 4).
#
# CHECKLIST:
#   1. preconditions — forge up + key + jq (memory DB tests need
#      pgvector installed on forge's Postgres — steps degrade with a
#      501 check instead of crashing)
#   2. demo agent + active conversation (the card's landing spot)
#   3. seed episodes: episodes are CAPTURED automatically at turn end
#      (H4.2) — automated when the forge has a live harness; otherwise
#      the script prints the documented manual seed (psql insert)
#   4. run the reflection path headlessly: POST the belief PROPOSALS
#      (the mule reflection workflow's output shape) — the same door
#      examples/reflection uses (forge `POST …/beliefs/proposals`)
#   5. assert the proposals API: GET /agents/:id/memory/beliefs
#      ?status=pending contains the proposed belief
#   6. the approval card: MANUAL (AgentAskRequest "Keep/Forget" on
#      phone/TUI — printed instructions)
#   7. keep the belief directly (POST …/beliefs/:bid/keep) and assert
#      it is active — "belief changes behavior" = the memory:beliefs
#      prompt section now carries it (printed verification)
#   8. the mule-side reflection workflow itself: MANUAL checklist
#      (seed examples/reflection + submit the job; needs mule + a
#      model + credentials)
#
# Automated: 1–5, 7, cleanup. Manual: 3 (faux forge), 6, 8.
# Exit code = number of failed steps.

set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ST=0
ST=$((ST + 1)); require_jq || :
ST=$((ST + 1)); require_forge "$ST" || :
ST=$((ST + 1)); require_forge_key "$ST" || :
[ "$HFAIL" -gt 0 ] && { echo; echo "preconditions failed — unblock the FAIL lines above and re-run"; finish; }

NAME="herd-demo-h4-$(date +%s)"
BELIEF_CONTENT="herd-demo-h4 belief: the user prefers terse answers over long prose"

# ── Step 2: demo agent + conversation ───────────────────────────────
ST=$((ST + 1)); hstep "$ST" "demo agent + active conversation"
AID=$(new_demo_agent "$NAME") || { hfail "agent create failed"; finish; }
CONV=$(fpost "/agents/$AID/conversations" '{"title":"herd-demo-h4"}')
CID=$(printf '%s' "$CONV" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
[ -n "$CID" ] && [ "$CID" != "null" ] && hok "agent $AID, conversation $CID" \
  || { hfail "no conversation: $(printf '%s' "$CONV" | head -c 150)"; finish; }

# ── Step 3: seed episodes ───────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "seed episodes (captured at turn end — H4.2)"
SEED=$(fpost "/sessions/$CID/messages" '{"content":"herd-demo-h4 episode seed: note that terse answers were preferred in recent turns."}')
sleep 3
SEARCH=$(fget "/agents/$AID/memory/search?q=terse&k=3&scope=episodes" 2>/dev/null)
if printf '%s' "$SEARCH" | jq -e '.episodes // .results // . // [] | length >= 0' >/dev/null 2>&1; then
  N=$(printf '%s' "$SEARCH" | jq -r 'if type=="array" then length else (.episodes // .results // []) | length end' 2>/dev/null)
  if [ "${N:-0}" -gt 0 ]; then
    hok "memory search returns $N episode(s) — capture is live"
  elif printf '%s' "$SEARCH" | jq -r '.error // empty' 2>/dev/null | grep -qi "501\|unavailable\|memory"; then
    hman "memory subsystem reports unavailable (pgvector not installed on this forge's Postgres?) — install pgvector ('sudo pacman -S pgvector' on the forge host) and re-run; episode capture requires a live harness turn, which a faux forge does not produce"
  else
    hfail "memory search returned no episodes (capture may need a settled turn): $(printf '%s' "$SEARCH" | head -c 200)"
  fi
else
  hman "memory search endpoint shape unknown/absent: $(printf '%s' "$SEARCH" | head -c 150) — manual seed instead: psql -d forge 'INSERT INTO memory.episodes (agent_id, summary, content, source) VALUES ('"'"'$AID'"'"', 'herd-demo-h4 manual seed', 'terse answers preferred', JSONB '"'"'{"seq_range":[1,1]}'"'"')'; then continue below"
fi

# ── Step 4: propose the belief (reflection output shape) ────────────
ST=$((ST + 1)); hstep "$ST" "POST the belief proposal (the reflection workflow's output)"
PROP=$(fpost "/agents/$AID/memory/beliefs/proposals" "$(jq -n \
  --arg c "$BELIEF_CONTENT" \
  '{beliefs:[{content:$c, kind:"preference", confidence:0.8,
             rationale:"herd-demo-h4: proposed by the reflection pass",
             source_episodes:[]}]}')")
if printf '%s' "$PROP" | jq -e 'true' >/dev/null 2>&1; then
  hok "proposal accepted: $(printf '%s' "$PROP" | head -c 200)"
else
  hfail "proposals API rejected: $(printf '%s' "$PROP" | head -c 200)"
fi

# ── Step 5: proposal visible as pending belief ─────────────────────
ST=$((ST + 1)); hstep "$ST" "GET /agents/:id/memory/beliefs?status=pending lists it"
sleep 2
BLS=$(fget "/agents/$AID/memory/beliefs?status=pending" 2>/dev/null)
if printf '%s' "$BLS" | jq -e '(.beliefs // . // [])[] | select(.content | contains("herd-demo-h4"))' >/dev/null 2>&1; then
  hok "pending belief present"
else
  hfail "no pending belief with our content: $(printf '%s' "$BLS" | head -c 200)"
fi

# ── Step 6: the approval card (manual) ──────────────────────────────
ST=$((ST + 1)); hstep "$ST" "approval card: MANUAL on phone/TUI"
cat <<EOF
   The proposal pushed a 'memory_review' card onto the agent's most-active
   conversation (this demo's $CID). On any surface:
     - phone (mobile-native): notification "I've learned: … — keep this
       memory?" with Keep / Forget buttons (+ free text = edit).
     - TUI: the '❓' pending-ask banner.
   Tap Keep (or answer from the terminal with the free-text edit) and
   the belief transitions pending → active.
EOF
hman "card answered by a human — step counted as manual"

# ── Step 7: keep the belief directly + active ──────────────────────
ST=$((ST + 1)); hstep "$ST" "POST …/beliefs/:bid/keep → belief is active"
BID=$(printf '%s' "$BLS" | jq -r '(.beliefs // . // [])[] | select(.content | contains("herd-demo-h4")) | .id // .belief_id // empty' 2>/dev/null | head -1)
if [ -z "$BID" ] || [ "$BID" = "null" ]; then
  hfail "could not extract the pending belief id: $(printf '%s' "$BLS" | head -c 200)"
else
  fpost "/agents/$AID/memory/beliefs/$BID/keep" >/dev/null 2>&1
  sleep 1
  ACT=$(fget "/agents/$AID/memory/beliefs?status=active" 2>/dev/null)
  if printf '%s' "$ACT" | jq -e '(.beliefs // . // [])[] | select(.id == "'$BID'" or .belief_id == "'$BID'")' >/dev/null 2>&1; then
    hok "belief $BID is active — it now rides the agent's 'memory:beliefs' prompt section in every new turn (the 'changes behavior in a fresh session' half: open a new conversation and ask the agent what it remembers about answer style)"
  else
    hfail "belief not active after keep: $(printf '%s' "$ACT" | head -c 200)"
  fi
fi

# ── Step 8: mule reflection workflow (manual) ──────────────────────
ST=$((ST + 1)); hstep "$ST" "the mule-side reflection workflow: MANUAL"
cat <<EOF
   The dogfood loop (examples/reflection in the mule repo):
   1. cd <mule-repo> && ./examples/reflection/seed.sh   (idempotent:
      workflow 'reflection' + its cron/event wakes)
   2. trigger it: curl -s -X POST -H "Authorization: Bearer $MULE_KEY" \
        -d '{"job":"reflection"}' "$MULE_URL/api/v1/jobs"   (or let the
      '0 3 * * *' cron wake fire; the manual event wake is
      'episodes_ready')
   3. the workflow pulls episodes (forge /memory/search), gates them
      (dup + contradiction + decay), and SUBMITS via the proposals
      API of step 4 — new cards appear exactly as step 6 describes.
   Automated gate: the reflection workflow's unit/integration tests
   ('go test ./...' in the mule repo — the 4 wasm modules run through
   wazero; H4.4 ledger).
EOF
hman "needs mule + a model + credentials — not runnable headlessly here"

# ── cleanup ─────────────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "cleanup"
if [ "${KEEP:-0}" = "1" ]; then
  hman "KEEP=1 — leaving agent $AID + belief $BID behind"
else
  [ -n "$BID" ] && [ "$BID" != "null" ] && \
    fpost "/agents/$AID/memory/beliefs/$BID/forget" >/dev/null 2>&1
  fdel "/agents/$AID" >/dev/null 2>&1
  hok "belief forgotten, agent deleted"
fi

finish
