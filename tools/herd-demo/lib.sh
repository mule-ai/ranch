# herd-demo shared helpers — sourced by herd-demo-h{1..6}.sh.
#
# Contract:
#   * every demo exits with the NUMBER OF FAILED CHECKLIST STEPS (0 = all
#     green; skipped/manual steps do not count, but are printed);
#   * environment:
#       RANCH_BIN   ranch binary (default: `ranch` on PATH)
#       FORGE_URL   forge API base (default: http://127.0.0.1:8080)
#       FORGE_KEY   forge API key, sk_forge_… (NO default — set it;
#                   forge's key page or your deploy config)
#       MULE_URL    mule API base (default: daemon.toml mule_url, else
#                   http://127.0.0.1:8080)
#       MULE_KEY    mule API key, sk_mule_… (NO default — set it)
#       KEEP=1      leave demo agents/wakes behind for inspection
#
#   Keys are deliberately NOT read from ~/.config/ranch/daemon.toml:
#   that file's forge_url is the lab forge, and the demo targets a
#   LOCAL forge by default — silently pointing the demo at lab
#   infrastructure with a lab key would make a "local" demo write lab
#   state. Set FORGE_URL/FORGE_KEY (and MULE_*) explicitly.

set -uo pipefail

HFAIL=0
HOK=0

# ── step printing ────────────────────────────────────────────────────
hstep() { echo; echo "── Step $1: $2"; }
hok()   { echo "   ok: $*"; HOK=$((HOK + 1)); }
hfail() { echo "   FAIL: $*"; HFAIL=$((HFAIL + 1)); }
hman()  { echo "   manual: $*"; }

finish() {
  echo
  echo "════════════════════════════════════════════"
  echo "result: $HOK ok, $HFAIL failed$( [ "$HFAIL" -gt 0 ] && echo ' — see FAIL lines above' )"
  exit "$HFAIL"
}

# ── environment resolution ───────────────────────────────────────────
RANCH_BIN="${RANCH_BIN:-ranch}"
FORGE_URL="${FORGE_URL:-http://127.0.0.1:8080}"
FORGE_KEY="${FORGE_KEY:-}"
_mule_url=""
if [ -f "$HOME/.config/ranch/daemon.toml" ]; then
  _mule_url=$(sed -n 's/^mule_url[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
    "$HOME/.config/ranch/daemon.toml" | head -1)
fi
MULE_URL="${MULE_URL:-${_mule_url:-http://127.0.0.1:8080}}"
MULE_KEY="${MULE_KEY:-}"

# ── precondition checks (fail loudly with the unblock instruction) ───
require_forge() {
  hstep "$1" "forge reachable at $FORGE_URL (GET /health)"
  if ! curl -sS -m 5 -o /dev/null "$FORGE_URL/health" 2>/dev/null; then
    hfail "forge not reachable at $FORGE_URL. Start a local forge (forge's \
justfile / systemd unit on this box, or docker) and/or point the demo at \
one: export FORGE_URL=http://HOST:PORT"
    return 1
  fi
  hok "forge health ok"
}

require_forge_key() {
  hstep "$1" "forge API key set (FORGE_KEY)"
  if [ -z "$FORGE_KEY" ]; then
    hfail "FORGE_KEY is unset. Create a key on the forge's key page \
(~/forge keys UI or POST /api-keys) and: export FORGE_KEY=sk_forge_…"
    return 1
  fi
  hok "FORGE_KEY set"
}

require_mule() {
  hstep "$1" "mule reachable at $MULE_URL (GET /health)"
  if ! curl -sS -m 5 -o /dev/null "$MULE_URL/health" 2>/dev/null; then
    hfail "mule not reachable at $MULE_URL. Start mule (docker compose up \
-d in the mule repo, or the fleet mule machine) and/or: \
export MULE_URL=http://HOST:PORT"
    return 1
  fi
  hok "mule health ok"
}

require_mule_key() {
  hstep "$1" "mule API key set (MULE_KEY)"
  if [ -z "$MULE_KEY" ]; then
    hfail "MULE_KEY is unset. Create a key (mule's /api-keys UI) and: \
export MULE_KEY=sk_mule_…"
    return 1
  fi
  hok "MULE_KEY set"
}

require_ranchd() {
  hstep "$1" "local ranch daemon reachable"
  if ! "$RANCH_BIN" ls >/dev/null 2>&1; then
    hfail "ranch daemon not reachable ($RANCH_BIN ls failed). Start it: \
systemctl --user start ranchd (or 'ranch daemon' in the foreground), \
then re-run this step."
    return 1
  fi
  hok "ranch daemon up"
}

# ── HTTP helpers (auth via Bearer key; bodies on stdout) ─────────────
fget()  { curl -sS -m 20 -H "Authorization: Bearer $FORGE_KEY" "$FORGE_URL$1"; }
fpost() { curl -sS -m 30 -X POST -H "Authorization: Bearer $FORGE_KEY" \
           -H 'Content-Type: application/json' -d "${2:-}" "$FORGE_URL$1"; }
fpatch(){ curl -sS -m 20 -X PATCH -H "Authorization: Bearer $FORGE_KEY" \
           -H 'Content-Type: application/json' -d "${2:-}" "$FORGE_URL$1"; }
fdel()  { curl -sS -m 20 -X DELETE -H "Authorization: Bearer $FORGE_KEY" "$FORGE_URL$1"; }

mget()  { curl -sS -m 20 -H "Authorization: Bearer $MULE_KEY" "$MULE_URL$1"; }
mpost() { curl -sS -m 30 -X POST -H "Authorization: Bearer $MULE_KEY" \
           -H 'Content-Type: application/json' -d "${2:-}" "$MULE_URL$1"; }
mdel()  { curl -sS -m 20 -X DELETE -H "Authorization: Bearer $MULE_KEY" "$MULE_URL$1"; }

# jq is required (precondition of every step that asserts JSON)
require_jq() {
  command -v jq >/dev/null 2>&1 || {
    echo "   FAIL: jq is required: install it (e.g. 'pacman -S jq')";
    HFAIL=$((HFAIL + 1)); return 1; }
}

# ── demo agent lifecycle (forge POST/DELETE /agents) ─────────────────
new_demo_agent() { # new_demo_agent <name> → echoes agent id, rc=1 on failure
  local name="$1" body id
  body=$(jq -n --arg n "$name" '{name:$n}')
  body=$(fpost "/agents" "$body") || return 1
  id=$(printf '%s' "$body" | jq -r '.id // .agent.id // empty' 2>/dev/null)
  [ -n "$id" ] && [ "$id" != "null" ] || {
    echo "   FAIL: POST /agents returned no id: $(printf '%s' "$body" | head -c 200)"
    return 1; }
  printf '%s' "$id"
}
