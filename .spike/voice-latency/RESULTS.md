# Voice-latency results (H6.4)

The two hard budgets for the voice loop (PLAN-HERD.md §H6.4):

| Budget | Meaning | Target |
|---|---|---|
| **TTS first chunk** | Time-to-first-bytes of the ONE-SENTENCE `POST /v1/audio/speech` — mule synthesizes the agent's reply sentence-by-sentence, so the phone's first audible chunk is exactly this request | **< 400 ms** |
| **STT partial** | Total time to transcribe a ≤ 4 s utterance WAV (`POST /v1/audio/transcriptions`) — the worst case a user can hold before endpointing closes the utterance | **< 800 ms** |

## Live numbers — PENDING

> **Honest status (2026-10-06):** this box has **no real Parakeet/Kokoro**
> (no GPU, no model weights — the forge proxies here are stubs/unavailable),
> so the numbers below are **not measured**. `measure.sh` is validated
> against a mock server (mechanics only: framing, timing, medians — the
> mock's latencies are fake and are NOT recorded here).
>
> To fill this table: on a box where forge runs real Parakeet-Tiny +
> Kokoro, run
> `./.spike/voice-latency/measure.sh $FORGE_URL $FORGE_API_KEY speech.wav`
> (a ~4 s 16 kHz mono WAV of real speech) and paste the output here.

| Measurement | Warm-up | Run 1 | Run 2 | Run 3 | Median | Budget | Pass |
|---|---|---|---|---|---|---|---|
| TTS-FIRST ttfb (1 sentence) | — | — | — | — | — | < 0.400 s | — |
| TTS-FIRST total (1 sentence) | — | — | — | — | — | (reference) | — |
| TTS-FULL total (~50 words) | — | — | — | — | — | (reference) | — |
| STT-PART total (4 s slice) | — | — | — | — | — | < 0.800 s | — |
| STT-FULL total (source WAV) | — | — | — | — | — | (reference) | — |

## Interpretation notes (fill in with the live run)

- **TTFB vs total for TTS-FIRST:** if the Kokoro proxy streams, TTFB is
  the real first-chunk latency. If it buffers the whole sentence, TTFB
  ≈ total for one sentence — the "first chunk" budget then applies to
  the single-sentence POST total (which is still much smaller than a
  paragraph POST; that is the point of mule's sentence-chunked TTS).
- **STT partials:** forge's STT proxy is request/response (no
  streaming Parakeet surface), so "partial" here means *a short
  utterance measured whole*. The mule-side complement is the early
  **steer** partial (first ~silence_ms pause → fire-and-forget POST of
  the utterance-so-far) — see mule spec.md "Voice loop v2 (H6.4)".
- **Box:** record CPU/GPU, forge version, model revisions, and whether
  the models were warm (the script's warm-up covers first-request JIT,
  not model load).
