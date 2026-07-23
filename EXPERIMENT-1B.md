# Experiment 1b — Token Economics & Kill Behavior (2026-07-23)

Closes Q1/Q2/Q5 carried from experiment 1. Method: ledger archaeology (every
agent's session JSONL records per-call usage — no reruns needed for costs) +
a live kill drill against a headless `omp -p` worker.

## Q1/Q2 — measured costs (exp1's real runs)

"New" = input + output + cacheWrite (the tokens that didn't exist before).
cacheRead listed separately — it's re-reading the cached prefix, far cheaper.

| worker | role | calls | new tokens | cacheRead |
|---|---|---:|---:|---:|
| DiffHunter | hunt, 54 commits | 43 | 152k | 2.9M |
| FixCpaLabelLeak | fix, UI 1-file | 12 | 52k | 0.4M |
| FixDisambigRace | fix, UI + extract | 16 | 60k | 0.6M |
| FixCpaOpening | fix, Rust + tests | 15 | 62k | 0.6M |
| FixCpaStaleness | fix, UI reactive | 13 | 69k | 0.5M |
| FixSkCpaClear | fix, cross-boundary | 26 | 96k | 1.4M |
| SkCpaUnitTest | test retrofit | 16 | 59k | 0.6M |
| VitestRegression | harness intro | 28 | 92k | 1.4M |
| E2ESignalk (died) | E2E harness pt.1 | 45 | 214k | 4.4M |
| E2EFinish | E2E harness pt.2 | 62 | 194k | 5.7M |
| headless `omp -p` drill | 7-step module+tests+bench | 11 | 58k | 0.4M |
| **TOTAL exp1** | 11 PRs, one day | 276 | **1.05M** | 18.6M |

**Headline numbers for the scheduler:**
- Fix+test+PR cycle: **50–100k new tokens** (median ~60k; cross-boundary = top end).
- Diff-focused hunt on a real repo: **~150k** ≈ 2.5 fix cycles.
- Harness/infra PRs: 90–400k — night-window work.
- **Session floor: ~32k cacheWrite on call #1** (system prompt + context
  caching) before any work happens. Caps below ~40k are meaningless; tiny
  jobs are disproportionately taxed — batch small work into fewer sessions.
- Headless `omp -p` costs match task-subagent costs (58k vs 52–96k band):
  no harness-mode penalty.
- Empirical window fit — **corrected by the window spike:** the exp1 day
  (~1.05M new + 18.6M cacheRead + the orchestrating session) fit in one day,
  but DID exhaust the 5h window once (`exhausted` 10:38 → reset 12:10,
  mid-exp1c). E2ESignalk's death (~10:31, exit 1) coincides — likely budget,
  not context; E2EFinish's 35m run absorbed the throttle via retries and
  recovered after reset. Lessons: a heavy day fills a 5h window; the pipeline
  SURVIVES exhaustion without losing work; and the 7-day cap (0.29 → 0.42 in
  this one day) is the true weekly budget. A one-great-PR night (hunt +
  2 fixes ≈ 270k new) remains a comfortable fraction of proven capacity.

## Q5 — kill drill (the watchdog pattern)

Design premise validated: **don't rely on harness caps — enforce externally.**
The session JSONL is written live, so the orchestrator can meter a worker in
real time and kill at threshold.

Drill: `omp -p` worker (multi-step task, git commit per step) in a scratch
repo, watchdog polling its JSONL every 2s, SIGTERM (via supervisor) at 20k.

Results:
- Threshold tripped on call #1 (32k cacheWrite floor — see above). Killed at
  22.9s, exit 143, clean process-tree termination.
- **Ledger integrity: zero unparseable lines** after SIGTERM; last record is
  a well-formed event. Live metering + post-mortem accounting both safe.
- Workspace: consistent at last completed step (here: nothing yet — killed
  during first write). With commit-per-step in the worker playbook, a killed
  worker leaves a resumable checkpoint chain — same recovery story as the
  E2E agent death in exp1c.

Harness-level caps that DO exist (belt to the watchdog's suspenders):
`task.softRequestBudget` (requests/spawn) and `task.maxRuntimeMs` (wall
clock), both hard-abort. Request-count is a decent proxy: exp1 workers
averaged ~3.5k new tokens/call, so e.g. budget 30 requests ≈ 100k new.

## Design consequences (fed into IDEA.md)

1. Worker contract gains: `budget` enforced by ledger-watch watchdog +
   SIGTERM; `task.maxRuntimeMs`-style wall clock as backstop.
2. Worker playbook: commit-per-step mandatory → preemption ≈ free.
3. Scheduler arithmetic: night window plans in units of ~60k (fix) and
   ~150k (hunt); day-window jobs sized 1 fix cycle max.
4. Session-floor amortization: prefer one worker doing 2–3 queued fixes
   over 3 workers doing one each (saves 2×32k floor) — unless parallelism
   wins the wall-clock race in a closing window.

## Status: CONCLUDED. Q1/Q2/Q5 answered.

Remaining open before the build: window observation (cold start/recovery),
chain-opener mechanics. Both belong to the window-chaining spike.
