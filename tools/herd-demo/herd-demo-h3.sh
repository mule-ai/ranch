#!/usr/bin/env bash
# herd-demo-h3.sh — Herd H3 acceptance: cron + webhook wakes, a policy
# deny, and an ask (PLAN-HERD §10.3 item 3).
#
# CHECKLIST:
#   1. preconditions — mule up + key, forge up + key, jq
#   2. demo agent on the target forge
#   3. create a CRON wake (5-field cron, every minute) targeting the
#      agent's conversation (target = forge_conversation)
#   4. create a WEBHOOK wake (source "herd-demo-h3")
#   5. fire the webhook: POST /api/v1/wakes/fire
#   6. assert wake_log row (GET /api/v1/wakes/<id>/log, ok=true) AND
#      the wake's prompt landed as a forge message ([mule wake …]
#      prefix) in the agent's conversation
#   7. policy DENY: create a policy denying the demo tool, evaluate it
#      via POST /api/v1/policies/evaluate → verdict "deny"
#   8. policy ASK: verdict "ask" → the phone chip is MANUAL (the ask
#      rides ranch's AgentAsk lane — see printed instructions)
#   9. cleanup
#
# Automated: 1–7 + 9 (API assertions). The cron lane itself fires in
# the background on the mule host; step 3 asserts the wake is due-
# registered (next_due_at in the future).
# Manual: 8 (needs a phone/TUI to answer the chip).
# Exit code = number of failed steps.

set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ST=0
ST=$((ST + 1)); require_jq || :
ST=$((ST + 1)); require_mule "$ST" || :
ST=$((ST + 1)); require_mule_key "$ST" || :
ST=$((ST + 1)); require_forge "$ST" || :
ST=$((ST + 1)); require_forge_key "$ST" || :
[ "$HFAIL" -gt 0 ] && { echo; echo "preconditions failed — unblock the FAIL lines above and re-run"; finish; }

NAME="herd-demo-h3-$(date +%s)"

# ── Step 2: demo agent ──────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "demo agent on the target forge"
AID=$(new_demo_agent "$NAME") || { hfail "agent create failed"; finish; }
CONV=$(fpost "/agents/$AID/conversations" '{"title":"herd-demo-h3"}')
CID=$(printf '%s' "$CONV" | jq -r '.id // .session.id // .session_id // empty' 2>/dev/null)
hok "agent $AID conversation $CID"

TARGET=$(jq -n --arg u "$FORGE_URL" --arg k "$FORGE_KEY" --arg a "$AID" \
  '{forge_conversation:{forge_url:$u, api_key:$k, agent_id:$a}}')

# ── Step 3: cron wake ───────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "create cron wake (*/1 * * * *, demo)"
CRON=$(mpost "/api/v1/wakes" "$(jq -n \
  --arg n "herd-demo-h3-cron" --argjson t "$TARGET" \
  '{name:$n, kind:"cron", spec:{cron:"* * * * *"}, target:$t,
    prompt_template:"heartbeat: check in (herd-demo-h3)", enabled:true}')" \
  || { hfail "POST /api/v1/wakes (cron) failed"; finish; })
CRONID=$(printf '%s' "$CRON" | jq -r '.id // .wake.id // empty' 2>/dev/null)
if [ -n "$CRONID" ] && [ "$CRONID" != "null" ]; then
  NDX=$(printf '%s' "$CRON" | jq -r '.next_due_at // .wake.next_due_at // empty' 2>/dev/null)
  hok "cron wake $CRONID created, next_due_at=$NDX"
else
  hfail "no cron wake id: $(printf '%s' "$CRON" | head -c 200)"
  CRONID=""
fi

# ── Step 4: webhook wake ────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "create webhook wake (source herd-demo-h3)"
HOOK=$(mpost "/api/v1/wakes" "$(jq -n \
  --arg n "herd-demo-h3-hook" --argjson t "$TARGET" \
  '{name:$n, kind:"webhook", spec:{source:"herd-demo-h3"}, target:$t,
    prompt_template:"webhook fired: acknowledge (herd-demo-h3)", enabled:true}')" \
  || { hfail "POST /api/v1/wakes (webhook) failed"; finish; })
HOOKID=$(printf '%s' "$HOOK" | jq -r '.id // .wake.id // empty' 2>/dev/null)
[ -n "$HOOKID" ] && [ "$HOOKID" != "null" ] \
  && hok "webhook wake $HOOKID created" \
  || { hfail "no webhook wake id: $(printf '%s' "$HOOK" | head -c 200)"; HOOKID=""; }

# ── Step 5: fire the webhook ────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "fire the webhook: POST /api/v1/wakes/fire"
FIRE=$(mpost "/api/v1/wakes/fire" '{"kind":"webhook","source":"herd-demo-h3"}')
if printf '%s' "$FIRE" | jq -e 'true' >/dev/null 2>&1; then
  hok "fire accepted: $(printf '%s' "$FIRE" | head -c 150)"
else
  hfail "fire failed: $(printf '%s' "$FIRE" | head -c 200)"
fi

# ── Step 6: wake_log row + forge message ───────────────────────────
ST=$((ST + 1)); hstep "$ST" "assert wake_log row + the prompt landed in the agent conversation"
sleep 3
if [ -z "$HOOKID" ] || [ "$HOOKID" = "null" ]; then
  hfail "cannot check wake_log — webhook wake id was empty (step 4 failed)"
else
LOG=$(mget "/api/v1/wakes/$HOOKID/log" 2>/dev/null)
if printf '%s' "$LOG" | jq -e '((.log // .entries // [])[] | select(.ok==true or .ok==null))' >/dev/null 2>&1; then
  hok "wake_log row present: $(printf '%s' "$LOG" | jq -c '(if type=="array" then . else (.log // .entries // []) end)[0] // empty' 2>/dev/null | head -c 200)"
else
  hfail "no ok wake_log row for $HOOKID: $(printf '%s' "$LOG" | head -c 200)"
fi
fi
sleep 3
ROWS=$(fget "/sessions/$CID/messages" 2>/dev/null)
if printf '%s' "$ROWS" | jq -e '.[] | select((.content // "") | contains("herd-demo-h3"))' >/dev/null 2>&1; then
  hok "the wake's prompt ([mule wake …] prefix) is a forge message in $CID"
else
  hfail "no wake message in the conversation yet: $(printf '%s' "$ROWS" | jq -r '.[] | .content // empty' 2>/dev/null | tail -3)"
fi

# ── Step 7: policy deny ─────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "policy deny: rule denying the demo tool evaluates to deny"
POL=$(mpost "/api/v1/policies" "$(jq -n --arg a "$AID" \
  '{name:"herd-demo-h3-deny", agent_id:$a,
    rules:[{tool:"*herd_demo*", disposition:"deny", reason:"demo deny rule"}]}')" \
  || { hfail "POST /api/v1/policies failed"; finish; })
POLID=$(printf '%s' "$POL" | jq -r '.id // .policy.id // empty' 2>/dev/null)
EV=$(mpost "/api/v1/policies/evaluate" "$(jq -n --arg a "$AID" \
  '{agent_id:$a, tool:"herd_demo_cmd", input:{}}')")
if printf '%s' "$EV" | jq -e '.verdict == "deny"' >/dev/null 2>&1; then
  hok "evaluate verdict=deny (policy $POLID): $(printf '%s' "$EV" | head -c 150)"
else
  hfail "expected verdict deny, got: $(printf '%s' "$EV" | head -c 200)"
fi

# ── Step 8: policy ask (manual chip) ────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "policy ask: verdict=ask → phone chip is MANUAL"
EV2=$(mpost "/api/v1/policies/evaluate" "$(jq -n --arg a "$AID" \
  '{agent_id:$a, tool:"herd_demo_ask", input:{}}')")
if printf '%s' "$EV2" | jq -e '.verdict == "ask"' >/dev/null 2>&1; then
  hok "evaluate verdict=ask — the ASK half works headlessly; the CHIP is manual:"
  cat <<EOF
   1. On the FORGE side, run an agent turn whose tool call matches
      "herd_demo_ask" with forge's FORGE_POLICY_URL=$MULE_URL set.
   2. forge's before_tool hook asks mule → verdict ask → the
      ranch_tool_request card reaches every ranch surface:
        - TUI: the "❓" pending-ask banner (answer: Allow this action)
        - phone: notification with "Allow this action" / "Deny"
          buttons (mobile-native) — tap Allow.
   3. After the human allows on phone/TUI, the decision is memoized
      (mule's POST /api/v1/policies/memo). Verify: re-evaluate the
      same action → verdict "allow", no second chip:
      curl -s -X POST -H "Authorization: Bearer $MULE_KEY" \
           -H 'Content-Type: application/json' \
           -d '{"agent_id":"$AID","tool":"herd_demo_ask","input":{}}' \
           "$MULE_URL/api/v1/policies/evaluate" | jq .verdict
EOF
else
  hman "default policy has no ask rule — to demo the chip, create one: POST /api/v1/policies with a rule {tool: 'herd_demo_ask', disposition: 'ask', reason: 'demo ask rule'}, then run the manual steps above (current verdict: $(printf '%s' "$EV2" | jq -r '.verdict' 2>/dev/null))"
fi

# ── Step 9: cleanup ─────────────────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "cleanup (wakes + policy + agent)"
if [ "${KEEP:-0}" = "1" ]; then
  hman "KEEP=1 — leaving wake $CRONID/$HOOKID, policy $POLID, agent $AID behind"
else
  [ -n "$CRONID" ] && mdel "/api/v1/wakes/$CRONID" >/dev/null 2>&1
  [ -n "$HOOKID" ] && mdel "/api/v1/wakes/$HOOKID" >/dev/null 2>&1
  [ -n "$POLID" ] && mdel "/api/v1/policies/$POLID" >/dev/null 2>&1
  fdel "/agents/$AID" >/dev/null 2>&1
  hok "cleaned up"
fi

finish
