#!/usr/bin/env bash
# herd-demo-h6.sh — Herd H6 acceptance: voice call with mid-task
# steer + barge-in ×10, specialist provisioning
# (PLAN-HERD §10.3 item 6).
#
# CHECKLIST:
#   1. preconditions — mule up + key, forge up + key, jq
#   2. voice-loop workflow present (mule GET /api/v1/workflows); if
#      not seeded, the manual seed steps are printed
#      (examples/voice-loop/seed.sh in the mule repo)
#   3. LIVE typed-text call via `ranch call <agent>` (the TUI-parity
#      fallback — type 'hello', assert an assistant frame 'agent: …',
#      hang up). Needs the local ranchd configured against this
#      mule; otherwise printed as manual with the exact command.
#      Mid-task steer (overlap a second utterance while the agent is
#      speaking) is the same command + a second line mid-TTS — the
#      mule v2 unit tests cover it; a real-mic steer is manual.
#   4. barge-in ×10: the AUTOMATED gate is mule's
#      TestCallTenConsecutiveInterruptions (+ TestCallBargeInStopsTTSAnd
#      CapturesSpeech) — run them here when the mule checkout is
#      available; the REAL-AUDIO 10× acceptance stays manual
#      (phone CallActivity, mic hot while TTS speaks).
#   5. specialist provisioning: `ranch agents new` wizard — fully
#      interactive, so the TYPED SEQUENCE is documented (not driven);
#      phone ＋Agent form = manual checklist.
#
# Automated: 1–4 (4 = mule unit tests). Manual: 3 (no live mule /
# no matching daemon config), 3's real-mic half, 4's real-hardware
# half, 5.
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

MULE_REPO="${MULE_REPO:-$HOME/src/mule}"

# ── Step 2: voice-loop workflow ─────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "voice-loop workflow seeded in mule"
WFS=$(mget "/api/v1/workflows" 2>/dev/null)
WF_NAME=$(printf '%s' "$WFS" | jq -r '((.workflows // . // [])[] | .name // empty) | select(contains("voice"))' 2>/dev/null | head -1)
if [ -n "$WF_NAME" ]; then
  hok "voice-loop workflow present: $WF_NAME"
else
  hman "no voice workflow listed — seed it manually:"
  cat <<EOF
   cd $MULE_REPO && ./examples/voice-loop/seed.sh
   (idempotent: creates the 'voice-loop' workflow with one
   voice_call step {agent_id, stt_url, tts_url, ...} — see its
   README for the STT/TTS endpoint config; then re-run this step.)
EOF
fi

# ── Step 3: live typed-text call ────────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "LIVE call: \`ranch call\` typed-text fallback — 'hello' → assistant frame"
DAEMON_MULE_URL=$(sed -n 's/^mule_url[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
  "$HOME/.config/ranch/daemon.toml" 2>/dev/null | head -1)
if [ "$DAEMON_MULE_URL" = "$MULE_URL" ] && "$RANCH_BIN" ls >/dev/null 2>&1; then
  AGENT_NAME=$(fget "/agents" | jq -r '((.agents // . // [])[] | .name // empty)' 2>/dev/null | head -1)
  if [ -n "$AGENT_NAME" ]; then
    OUT=$(timeout 90 bash -c "printf 'hello\nbye\n' | \"$RANCH_BIN\" call '$AGENT_NAME' 2>&1" || true)
    if printf '%s' "$OUT" | grep -q '^agent: '; then
      hok "assistant frame received: $(printf '%s' "$OUT" | grep '^agent: ' | head -1 | head -c 120)"
    else
      hfail "no 'agent: …' assistant frame in the call output: $(printf '%s' "$OUT" | tail -5 | tr '\n' ' ' | head -c 300)"
    fi
  else
    hman "no forge agent to call — create one first (herd-demo-h1.sh)"
  fi
else
  hman "local ranchd not configured against this mule (daemon.toml mule_url=${DAEMON_MULE_URL:-unset} vs $MULE_URL). Manual:"
  cat <<EOF
   1. ensure the daemon's mule config points here (daemon.toml mule_url/mule_key)
   2. ranch call <agent>        (typing mode when no mic tools)
   3. type: hello   → expect "agent: …" (the assistant frame)
   4. mid-task steer: while the reply is still streaming/speaking,
      type a second line (e.g. "and stay under 20 words") — mule v2
      submits it fire-and-forget into the in-flight turn.
   5. type: bye      → hang-up
EOF
fi

# ── Step 4: barge-in ×10 (automated mule gate + manual) ─────────────
ST=$((ST + 1)); hstep "$ST" "barge-in ×10: mule unit tests (automated gate)"
if [ -d "$MULE_REPO" ] && command -v go >/dev/null 2>&1; then
  if (cd "$MULE_REPO" && timeout 300 go test ./internal/voice/ \
       -run 'TestCallTenConsecutiveInterruptions|TestCallBargeInStopsTTSAndCapturesSpeech' -count=1 2>&1 \
     | tail -3 | grep -q 'ok'); then
    hok "mule barge-in tests green (10/10 consecutive interruptions, stop-to-flush < 150 ms)"
  else
    hfail "mule barge-in tests failed — run by hand: cd $MULE_REPO && go test ./internal/voice/ -run 'TestCallTenConsecutiveInterruptions' -count=1"
  fi
else
  hman "mule checkout/go toolchain not available here ($MULE_REPO missing or no go) — automated gate: go test ./internal/voice/ -run 'TestCallTenConsecutiveInterruptions|TestCallBargeInStopsTTSAndCapturesSpeech'"
fi
cat <<EOF
   REAL-HARDWARE 10× (manual, phone CallActivity): answer a call, and
   while the TTS is speaking interrupt mid-sentence 10 times in a row
   (speak over the speaker): each interruption must stop the TTS
   within ~150 ms, no echo feedback, and the 10th interruption still
   captures a clean utterance.
EOF

# ── Step 5: specialist provisioning ─────────────────────────────────
ST=$((ST + 1)); hstep "$ST" "specialist-dot provisioning: wizard typed sequence + phone form (MANUAL)"
cat <<EOF
   TUI/CLI — the wizard is fully interactive; the typed sequence
   ('ranch agents new', answers in [] are the defaults):
     [name]        billing
     [avatar]      (paste a URL or skip)
     [profile]     pick the number of a read-leaning profile (no
                   bash/edit in its tools)
     [tools]       allowlist: leave empty = allow all, or comma-
                   list the allowed tools
     [memory]      org  [org label]  default
     [home]        (this machine)
     [wake]        07:00      → creates the daily cron wake
   Assert afterwards: GET /agents lists 'billing'; mule
   GET /api/v1/wakes shows the cron wake; the 24 h proactive-turn
   acceptance = its heartbeat wake has produced ≥ 1 turn by
   morning (check GET /agents/<id>/conversations).

   PHONE (manual checklist): mobile-native registry screen → '＋'
   (NewAgentActivity: baseline form — name/profile/memory/wake) →
   the AgentProvision frame runs the SAME provision() core in the
   daemon. Verify: new agent row on both phone + TUI, wake in mule.
EOF
hman "provisioning is a UX flow — verify on TUI + phone"

finish
