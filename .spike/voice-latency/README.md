# voice-latency spike (H6.4)

Measures the voice loop's two hard budgets against a **forge STT/TTS
proxy** (Parakeet + Kokoro — the same two `POST /v1/audio/…` endpoints
mule's `internal/voice` uses):

- **TTS first chunk** — time-to-first-bytes of a one-sentence
  `POST /v1/audio/speech` (mule synthesizes replies sentence-by-sentence,
  so the phone's first audible chunk is this request). Budget **< 400 ms**.
- **STT partial** — total time to transcribe a ≤ 4 s utterance WAV.
  Budget **< 800 ms**.

Files:

- `measure.sh` — the measurement (curl + python3 stdlib only; no
  dependencies). `./measure.sh FORGE_URL FORGE_API_KEY [speech.wav]`.
  Without a WAV it synthesizes a 4 s 440 Hz sine (latency scales with
  audio duration, not content — fine for the pipeline measurement).
  Each metric: 1 warm-up + 3 runs, median reported. TTFB *and* total
  are both reported, because whether TTFB is meaningful depends on
  whether the Kokoro proxy streams (see RESULTS.md "Interpretation").
- `RESULTS.md` — the results template + budget table + **honest status:
  live numbers are pending** (this box has no real Parakeet/Kokoro; the
  script is validated against a mock, and the mock's latencies are
  deliberately not recorded).

## Run it (where the real proxies exist)

```bash
cd ~/src/ranch-2/.spike/voice-latency
# ~4 s of real speech, 16 kHz mono s16le WAV (phone "Save recording"
# re-sampled, or `arecord -f cd -d 4` then convert)
./measure.sh http://host:8110 sk_forge_… speech.wav
```

Paste the output into RESULTS.md on the same box/forge, then update
PLAN-HERD.md's H6.4 ledger with the pass/fail against the two budgets.

## What this does NOT measure

- The **mule-side** stop-to-flush barge-in latency (< 150 ms) — that
  one is asserted in `mule`'s `internal/voice` unit tests against a
  fake TTS streamer (no real models needed): `go test ./internal/voice/
  -run 'TestCallTenConsecutiveInterruptions|TestCallDualChannelLiveness'`.
- The **phone tail** (wire + AudioTrack queue drain) — client-side,
  documented in mule spec.md "Voice loop v2 (H6.4)"; the `{"flush":true}`
  control frame is the phone's cue to drop its playback buffer.
