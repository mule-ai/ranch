# Herd end-to-end demo scripts (PLAN-HERD §10.3)

One shell script per Herd phase, run by hand against a LOCAL forge (+
mule where the phase needs it). CI-optional: each script is a
checklist with numbered, echoed steps; **exit code = number of failed
steps**. Skipped/manual steps are printed, not counted.

```
herd-demo-h1.sh   agent CRUD + talk/continue/fork (API + `ranch agent` CLI)
herd-demo-h2.sh   subagent spawn/abort via a forged prompt (+ manual:
                  kill -9 ×3, timer across restart, compaction)
herd-demo-h3.sh   cron + webhook wakes, policy deny + ask
herd-demo-h4.sh   episodes → reflection proposals → belief approval
herd-demo-h5.sh   proactive research, memory-trigger queue, Activity
                  View parity (manual), herd pause + 409
herd-demo-h6.sh   voice call (typed-text fallback), barge-in ×10
                  (mule unit tests = the automated gate), specialist
                  provisioning
```

## Environment

| var | default | notes |
|---|---|---|
| `RANCH_BIN` | `ranch` | the CLI binary |
| `FORGE_URL` | `http://127.0.0.1:8080` | a LOCAL forge instance |
| `FORGE_KEY` | *(unset = failed step)* | `sk_forge_…`; never auto-read from `~/.config/ranch/daemon.toml` — that file points at the LAB forge, and a "local" demo silently writing lab state would be worse than failing |
| `MULE_URL` | daemon.toml `mule_url`, else `http://127.0.0.1:8080` | |
| `MULE_KEY` | *(unset = failed step)* | `sk_mule_…` |
| `KEEP=1` | — | leave the demo agents/wakes/beliefs behind for inspection |
| `MULE_REPO` | `~/src/mule` | h6 runs the barge-in tests from here |

`jq` is required. Precondition checks fail loudly with the unblock
command (start local forge / export the key / start mule …).

Honesty contract: steps that need a human surface (phone chips,
Activity View eyeball, real-mic voice, the interactive wizard) are
printed as `manual:` with the exact commands/typed sequences — they
are never silently skipped and never counted as failures.
