# Experiment 2 — Window Spike (2026-07-23)

Closes the last two open questions (cold start/recovery, chain opener).
Ran same-day, daytime — night was never a requirement, only a quiet boundary.

## Findings

1. **Account window state is local and structured.**
   `~/.omp/agent/agent.db : usage_history` records `used_fraction`, `status`,
   `resets_at` per limit: `anthropic:5h`, `anthropic:7d`, `anthropic:7d:fable`
   (per-model-class!). This is the cold-start/recovery signal, the ops-strip
   data source, and the throttle detector, all in one table.
   **Caveat: it is refreshed only by omp's own sparse probe cycle** (3 probes
   all day today). The orchestrator must force its own probes via the OAuth
   `/usage` endpoint (token in `auth_credentials`) when it needs freshness.

2. **The 7-day cap is the true budget.** One heavy exp1 day: `anthropic:7d`
   0.29 → 0.42 (and `7d:fable` 0.42). 5h windows are the scheduling grain;
   the weekly fraction decides how much any window may burn. Scheduler rule
   in IDEA.md: `(7d remaining − interactive reserve) / windows-until-reset`.

3. **We were throttled today and the pipeline survived.** 5h window
   `exhausted` 10:38 → reset 12:10, mid-exp1c. One worker died at the
   boundary (E2ESignalk — previously misdiagnosed as context exhaustion),
   its successor absorbed the gap via retries and salvaged the workspace.
   Exhaustion is a survivable, observable event — not a failure mode to fear.

4. **Reset semantics confirmed from observation:** window = 5h from first
   request; `resets_at` exact while active; fresh window row (`0.0`,
   `resets_at: None`) stamped at the boundary (12:10:10).

5. **Opener cost kills the "cheap message" idea.** Minimal `omp -p` opener
   ("Reply with exactly: ok"): 1 call, 2 in + 4 out + **31,678 cacheWrite**,
   6.6s — the ~32k session floor IS the opener. ×33 windows/week ≈ 1M
   tokens/week of pure hello. Design consequence:
   - **The first real job opens the window.** Its session floor was owed
     anyway; a dedicated opener is pure waste when the queue is non-empty.
   - Empty queue → direct Anthropic API call with subscription OAuth
     (no system prompt, ~10 tokens). Implement in the orchestrator; not
     needed for v1 if the queue is kept non-empty (hunt tasks are infinite).

6. **Watcher bug worth remembering:** a stale `resets_at: None` row misled
   the first watcher run into firing early. `resets_at: None` means "window
   not yet active / stale snapshot", NOT "past reset". Probe before trusting.

## Status: CONCLUDED. All pre-build questions are now closed.

Design phase complete. Everything the server automates has been executed
and measured by hand: hunt, triage, fix, evidence, PR, verdict, kill,
resume, window arithmetic. Next: build the orchestrator.
