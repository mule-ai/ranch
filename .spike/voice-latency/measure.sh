#!/usr/bin/env bash
# H6.4 voice-latency spike — measure the voice loop's two hard budgets
# against a forge STT/TTS proxy (Parakeet + Kokoro).
#
# Usage:
#   ./measure.sh FORGE_URL FORGE_API_KEY [speech.wav]
#
#   FORGE_URL     e.g. http://host:8110 (the mule config's forge_url)
#   FORGE_API_KEY sk_forge_…
#   speech.wav    optional: a 16 kHz mono s16le WAV of real speech
#                 (~4 s). Without one, a 440 Hz sine is synthesized
#                 (latency is measured on duration, not content —
#                 Parakeet's inference time scales with audio length).
#
# What is measured (budgets from docs/PLAN-HERD.md §H6.4 + mule
# spec.md "Voice loop v2"):
#
#   TTS-FIRST  time-to-first-bytes of a ONE-SENTENCE
#              POST /v1/audio/speech — mule synthesizes the agent's
#              reply sentence-by-sentence, so the first out-chunk the
#              phone hears is exactly this request. Budget < 400 ms.
#              (Honesty note: if the Kokoro proxy does not stream,
#              TTFB ≈ full-response time for one sentence; the
#              sentence-chunking at the mule layer is what makes this
#              the "first chunk". TTFB and total are both reported.)
#   TTS-FULL   total time for a ~50-word paragraph POST — the v1
#              without chunking; reference only.
#   STT-PART   total time to transcribe a ≤4 s utterance WAV (the
#              worst-case "partial" a user can hold before endpointing
#              closes it). Budget < 800 ms.
#
# Each measurement is warmed up once, then taken 3 times; the median
# is reported. No numbers are invented: against a forge without real
# Parakeet/Kokoro the script reports HTTP errors instead.

set -euo pipefail

if [ $# -lt 2 ]; then
  sed -n '2,30p' "$0"
  exit 1
fi
FORGE_URL=${1%/}
KEY=$2
WAV=${3:-}

BASE="${FORGE_URL}/v1/audio"
AUTH=(-H "Authorization: Bearer ${KEY}")

FIRST_SENTENCE='On it — checking the deploy now.'
FULL_PARAGRAPH='All right, here is where things stand. The build is green and the migration has been applied. Two agents are waiting on review, and the reflection workflow finished overnight with three memory updates pending your approval. I can start the next task whenever you are ready, or we can look at the open issues first. Just say the word and I will get moving.'

# --- a 440 Hz sine WAV if none was provided -----------------------------
if [ -z "$WAV" ] || [ ! -f "$WAV" ]; then
  WAV=$(mktemp -t voice-lat-XXXXXX).wav
  echo "no speech.wav given — synthesizing a 4 s 440 Hz sine at $WAV" >&2
  python3 - "$WAV" <<'PY'
import math, struct, sys
path = sys.argv[1]
rate, secs, hz = 16000, 4, 440
n = rate * secs
frames = b"".join(
    struct.pack("<h", int(12000 * math.sin(2 * math.pi * hz * i / rate))) for i in range(n)
)
with open(path, "wb") as f:
    f.write(b"RIFF" + struct.pack("<I", 36 + len(frames)) + b"WAVEfmt ")
    f.write(struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16))
    f.write(b"data" + struct.pack("<I", len(frames)) + frames)
PY
fi

# --- timing helpers ------------------------------------------------------
# timeit url label [curl args...] -> "ttfb total http_code"
timeit() {
  local url=$1 label=$2; shift 2
  curl -s -o /dev/null -w "%{time_starttransfer} %{time_total} %{http_code}" \
    "${AUTH[@]}" "$@" "$url" 2>/dev/null
}

median3() { # three "0.123" values on stdin (or args) -> median
  printf '%s\n' "$@" | sort -n | sed -n '2p'
}

run3() { # label curl-args... ; prints "median-ttfb median-total http"
  local label=$1 url=$2; shift 2
  local a b c
  timeit "$url" "$label" "$@" > /dev/null  # warm-up (model load / JIT)
  a=$(timeit "$url" "$label" "$@"); b=$(timeit "$url" "$label" "$@"); c=$(timeit "$url" "$label" "$@")
  local ttfb total code
  ttfb=$(median3 "$(awk '{print $1}' <<<"$a")" "$(awk '{print $1}' <<<"$b")" "$(awk '{print $1}' <<<"$c")")
  total=$(median3 "$(awk '{print $2}' <<<"$a")" "$(awk '{print $2}' <<<"$b")" "$(awk '{print $2}' <<<"$c")")
  code=$(awk '{print $3}' <<<"$c")
  echo "$label $ttfb $total $code"
}

tts_body() { # text -> json body
  printf '{"model":"kokoro","input":%s,"voice":"am_adam","response_format":"pcm","speed":1}' \
    "$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1")"
}

echo "=== voice-latency spike: forge=$FORGE_URL ==="
echo

# --- TTS -----------------------------------------------------------------
FIRST_BODY=$(tts_body "$FIRST_SENTENCE")
FULL_BODY=$(tts_body "$FULL_PARAGRAPH")
echo "--- TTS (POST $BASE/speech) ---"
run3 "TTS-FIRST (1 sentence, the phone's first chunk)" "$BASE/speech" \
  -H 'Content-Type: application/json' -d "$FIRST_BODY"
run3 "TTS-FULL  (~50 words, reference)" "$BASE/speech" \
  -H 'Content-Type: application/json' -d "$FULL_BODY"
echo

# --- STT -----------------------------------------------------------------
WAV_SECS=$(python3 -c "
import wave, sys
w = wave.open(sys.argv[1])
print(f'{w.getnframes()/w.getframerate():.1f}')
" "$WAV")
echo "--- STT (POST $BASE/transcriptions, ${WAV_SECS}s WAV) ---"
run3 "STT-FULL ($WAV_SECS s)" "$BASE/transcriptions" \
  -F "file=@$WAV" -F "model=parakeet" -F "response_format=json"

# A ≤4 s slice is the worst-case "partial": cut it with python if the
# source is longer than 4 s.
SLICE=$(mktemp -t voice-lat-slice-XXXXXX).wav
python3 - "$WAV" "$SLICE" <<'PY'
import sys, wave
src, dst = sys.argv[1], sys.argv[2]
s, d = wave.open(src), wave.open(dst, "wb")
n = min(s.getnframes(), s.getframerate() * 4)
d.setnchannels(s.getnchannels()); d.setsampwidth(s.getsampwidth())
d.setframerate(s.getframerate()); d.writeframes(s.readframes(n))
PY
echo "--- STT partial (4 s slice — budget case) ---"
run3 "STT-PART  (4 s slice)" "$BASE/transcriptions" \
  -F "file=@$SLICE" -F "model=parakeet" -F "response_format=json"
echo
echo "Budgets: TTS-FIRST ttfb < 0.400 s | STT-PART total < 0.800 s"
echo "Paste the numbers into RESULTS.md (same box, same forge)."
