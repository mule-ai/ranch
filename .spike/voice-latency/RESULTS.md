# Voice-latency results (H6.4)

The two hard budgets for the voice loop (PLAN-HERD.md §H6.4):

| Budget | Meaning | Target |
|---|---|---|
| **TTS first chunk** | Time-to-first-bytes of the ONE-SENTENCE `POST /v1/audio/speech` — mule synthesizes the agent's reply sentence-by-sentence, so the phone's first audible chunk is exactly this request | **< 400 ms** |
| **STT partial** | Total time to transcribe a ≤ 4 s utterance WAV (`POST /v1/audio/transcriptions`) — the worst case a user can hold before endpointing closes the utterance | **< 800 ms** |

## Live numbers — MEASURED 2026-10-10 (real models, GPU)

> **Run:** `measure.sh` against the restored lab voice services on `srv`
> (RTX 4000 SFF Ada, NixOS LXC CT 201 — `3f5a902d`-era GPU-passthrough
> config restored from git history, lab commits `0a0495a4`–`60dc90e7`),
> run **direct at the services** (`http://10.10.199.51:5093` Parakeet,
> `http://10.10.199.51:8766` Kokoro — both are OpenAI-compatible on the
> exact proxy paths, so the forge-proxy hop adds nothing material; the
> 404 rows for the *other* service's endpoint are expected). STT model:
> Parakeet-TDT-0.6B-v3 FP32 (GPU-aware alias, CUDAExecutionProvider);
> TTS: Kokoro ONNX, `am_adam`, `response_format=pcm` (non-streaming —
> TTFB = first byte of the single-sentence POST, which is exactly what
> the phone's first chunk is). Test input: 2 s 440 Hz sine WAV + the
> script's 4 s slice (latency is measured on duration, not content).
> `nvidia-smi` after the probes: parakeet 3680 MiB + kokoro 450 MiB on
> the RTX 4000 (third consumer alongside embeddings — the original
> shared-GPU configuration). Medians of 3 runs after a warm-up run.

| Measurement | Warm-up | Runs | Median | Budget | Pass |
|---|---|---|---|---|---|---|---|
| TTS-FIRST ttfb (1 sentence) | 1 run | median of 3 | **0.283 s** | < 0.400 s | **PASS** |
| TTS-FIRST total (1 sentence) | — | — | 0.291 s | (reference) | — |
| TTS-FULL total (~50 words) | — | — | 2.052 s | (reference) | — |
| STT-PART total (4 s slice) | 1 run | median of 3 | **0.070 s** | < 0.800 s | **PASS** |
| STT-FULL total (source WAV, 2 s) | — | — | 0.069 s | (reference) | — |

> Both budgets pass with wide margin: TTS first chunk **283 ms vs the
> 400 ms budget**; STT partial **70 ms vs the 800 ms budget** (Parakeet
> TDT-0.6B runs ~57× real-time on the RTX 4000 — the STT hop will never
> be the latency bottleneck; the mule-side early-steer partial is
> therefore mostly a *completeness* feature, not a latency one).

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
- **Box:** srv (Intel + NVIDIA RTX 4000 SFF Ada, sm_89), NixOS LXC CT 201,
  unprivileged + device-node passthrough. Models warm after the
  warm-up run (model load happens at service boot, ~12 min first-boot
  install, not measured here). Forge version N/A — direct service runs;
  rerun through a forge with `PARAKEET_URL`/`KOKORO_URL` set to capture
  the proxy hop (expected: +single-digit ms, same host pair as the
  mule voice loop's actual path).
