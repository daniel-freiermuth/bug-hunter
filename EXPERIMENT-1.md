# Experiment 1 — One Manual Loop, Measured

Run the entire pipeline once, by hand, against one real repo. No server, no
scheduler, no UI. Every number this project's design depends on falls out of
this single run.

## Questions this answers

| # | Question | Feeds |
|---|----------|-------|
| Q1 | Tokens for a hunt pass that yields credible candidates? | day-window job sizing (≤20% allocation realistic?) |
| Q2 | Tokens for one fix+test+PR cycle? | PRs-per-night-window (guess: 1–2) |
| Q3 | Candidate yield & quality: how many findings, how many survive human triage? | signal-to-noise reality check |
| Q4 | Which evidence rung does a real fix land on? | ladder calibration |
| Q5 | What happens when a hard cap hits mid-job? | preemption / checkpoint design |
| Q6 | Is `findings.json` schema v0 sufficient? | store + UI schema |

## Repo selection criteria

Pick ONE repo, mine, that has:
- push access (PR against my own repo — no fork dance in experiment 1),
- a test suite that runs locally and is green at HEAD (verify first — a bug
  hunt on a broken baseline measures nothing),
- recent commit activity (last 2–4 weeks) — feeds the diff-focused strategy,
- non-trivial size (a weekend toy flatters the hunter).

Chosen repo: `___________________` (fill in at run time)

## Protocol

### Phase 0 — Baseline (no agent, ~10 min)

1. Fresh clone/worktree at HEAD. Build. Run suite. Record: green? duration?
2. Note the token counter start (session `budget.spent()` is per-session, so
   each phase below runs in its OWN session — spent = cost of that phase).

**Run log (2026-07-23):** repo = `daniel-freiermuth/winga-chart-plotter`
(Rust core → WASM + Svelte 5/deck.gl app; last commit 75 min before clone).
Baseline GREEN: `cargo test` 97 pass (~35s cold); `svelte-check` 0 errors /
0 warnings over 933 files (wasm-pack build 64s + check 18s). TS side has no
unit test harness → TS findings target rungs 2–3, as anticipated.

### Phase A — Hunt (capped session #1)

- Cap: hard, parameter `HUNT_CAP` — start guess **150k tokens**.
- Strategy: **diff-focused only** (highest hit rate per token; other playbooks
  are later experiments). Scope: commits from the last ~3 weeks.
- **Run deviation:** hunt runs as an OMP subagent — no hard token cap
  available on subagents from the orchestrating session; cost recorded from
  usage reporting instead. Cap enforcement test (Q5) deferred to 1b.
- Playbook prompt (v0):
  > You are hunting for latent bugs, not style issues. Examine the diff of the
  > last 3 weeks of commits and the code they touch. Look for: broken edge
  > cases, error paths that drop data, off-by-one/boundary mistakes, contract
  > drift between callers and callees, races introduced by recent changes.
  > For each candidate produce a `findings.json` entry (schema below). Rank by
  > severity × confidence. Do NOT fix anything. Quality over quantity: five
  > credible findings beat twenty guesses. For each, state how you would prove
  > it (which evidence rung is reachable).
- Output: `findings.json`, ranked.
- Record: tokens spent, wall time, #candidates.

### Phase B — Human triage (~10 min, timed)

- Read `findings.json`. For each: verdict `credible | reject | duplicate-ish`.
- Record: #credible, rejection reasons (verbatim — these seed the future
  suppression DB and hunt-prompt "known non-bugs" list).
- Pick the single best finding for Phase C.

### Phase C — Fix (capped session #2, fresh session)

- Cap: hard, parameter `FIX_CAP` — start guess **300k tokens**.
- Input: the one chosen finding + its evidence plan. New branch.
- Instructions (v0):
  > Prove the bug first: write a failing test if the code admits one
  > (rung 1); else a reproduction script (rung 2); else a written data-flow
  > trace (rung 3). Only then fix it, minimally. Run the affected package's
  > tests, then the full suite. Produce: branch with test+fix commits, and a
  > PR description stating: what breaks, why, the INTENDED behavior and how
  > you know it's intended, evidence rung, and what you did NOT change.
- Record: tokens spent, wall time, rung achieved, suite result.

### Phase D — Review & ship (~15 min)

- Review the branch as if a colleague sent it. Verdict: merge / rework / reject.
- If merge-worthy: open the PR for real. That PR is the experiment's trophy.

## Results (fill in)

| Metric | Value |
|--------|-------|
| Phase A tokens / wall time | tokens n/a (subagent usage not exposed to orchestrator — instrument in 1b); wall 9m50s |
| Candidates found / credible after triage | 5 / 5 (zero rejects; #5 kept as queue note) |
| Rejection reasons | none — suppression corpus still empty |
| Phase C tokens / wall time | 4 fixers in parallel: 3m29s / 4m00s / 4m24s / 10m52s (longest = cross-boundary Rust+TS) |
| Evidence rung achieved | #3→rung 1 (2 new cargo tests); #2→rung 1+3 (1 new test + trace); #1,#4→rung 3 (exhaustive traces) |
| Suite green? | yes, independently re-run: 99 & 98 cargo tests; svelte-check 0/0 + eslint clean on both TS branches |
| Human verdict on PR | all 5 approved → shipped as draft PRs #5–#9 (merge verdicts pending on GitHub) |
| Cap hit anywhere? Behavior? | not tested — caps unavailable on subagents; deferred to 1b |

**Scope deviation:** triage verdict was "all credible" → Phase C ran on 4
findings in parallel worktrees instead of 1. Bonus data: parallel fix fan-out
works; cross-boundary fixes dominate wall time.

**Notable Phase C discovery:** the skCpa fix required more than the hunter
predicted — serde_wasm_bindgen maps `None`→`undefined` (not `null`), so the
skip-attr removal alone wouldn't have fixed the runtime path. The fixer found
and solved this (`serialize_missing_as_null(true)`). Evidence-first protocol
caught a would-be-ineffective fix.

**Outcome (2026-07-23): SUCCESS.** 5 hunted, 5 survived triage, 5 fixed, 5
draft PRs opened the same day:
[#5](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/5) CPA
opening misclassification (rung 1) ·
[#6](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/6) SK CPA
retraction (rung 1+3) ·
[#7](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/7) stranded
CPA label (rung 3) ·
[#8](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/8)
disambig wrong-vessel race (rung 3) ·
[#9](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/9) CPA
own-state staleness (rung 3; #5 was promoted from queue note by the human —
verdict flow works in both directions).
Findings #5–#9 all cluster around one feature commit (c615344, two-tap AIS +
CPA): fresh feature code is where the bugs are — strong validation of the
diff-focused strategy.

**Ops lessons for the server design:** (1) PR-DESCRIPTION.md must NOT be
committed into fix branches — extract as PR body (fixed mid-run, encode in
fixer playbook). (2) Disk is a real resource: per-worktree Rust target dirs +
node_modules filled a 99%-full disk and stalled pnpm; the server needs
checkout hygiene (shared caches, cleanup after ship). (3) pnpm-via-copied
node_modules breaks `pnpm run`'s deps check — invoke tool binaries directly.

## Addendum — experiment 1c: closing the test gap (same day)

PRs #7–#9 shipped at rung 3 because the TS layer had no harness. Policy
decided (now in IDEA.md): fixers never introduce frameworks; harness PRs are
their own deliverable. Both remediation tracks ran in parallel:

- **[#10](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/10)
  vitest harness** + regression test stacked onto PR #8 (12/12; failure on
  legacy semantics demonstrated by actual revert, 3/5 fail). PR #9 unit test
  skipped with written justification — property inseparable from map/WASM.
- **[#11](https://github.com/daniel-freiermuth/winga-chart-plotter/pull/11)
  Playwright + mock-SignalK E2E harness**: all three bugs reproduced on main
  (S1 label persists, S2 wrong-vessel 3/3, S3 label frozen 15s), 3/3 green
  with fixes merged. PRs #7/#8/#9 upgraded from rung 3 to rung 2 via linked
  reproductions.

Extra knowledge the E2E run produced: bug #5 is masked in SK-nav mode (WS
traffic coincidentally re-runs the CPA effect); it manifests in geolocation
mode — impact scoped on PR #9. And S2's spec observed the disambig popup
*listing* wrong vessels at build time on main — same bug class, one more
data point that the id-keyed fix was the right shape.

Agent-ops lessons: the first E2E agent died of context exhaustion mid-build —
its uncommitted scaffold was fully salvageable; a continuation agent primed
with the predecessor's digested recon finished in one pass. The server's
worker design needs exactly this: durable workspace + resumable handoff notes,
not restart-from-zero.

## findings.json schema v0 (under test — Q6)

```json
{
  "fingerprint": "repo:path/file.ext:symbol:bug-class",
  "file": "path/file.ext",
  "symbol": "functionOrMethod",
  "bug_class": "boundary|error-path|race|contract-drift|leak|logic",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence",
  "evidence_plan": "how to prove it + expected rung (1-3)",
  "introduced_by": "commit sha if diff-focused hunt identified it"
}
```

## Success / failure criteria

- **Success:** ≥1 finding survives triage AND Phase C produces a rung ≤2 fix
  with a green suite that I would actually merge. Costs recorded for Q1/Q2.
- **Partial:** credible findings but Phase C fails (cap, wrong fix, rung 3
  only) — still valuable: Q1–Q5 answered, fix loop needs iteration.
- **Failure:** zero credible candidates from a healthy active repo. Then the
  hunt playbook is the problem — iterate prompt/strategy before building ANY
  infrastructure. (detail.dev's results say the bugs exist; failure here means
  our playbook can't see them yet.)

## Conclusion (2026-07-23) — CONCLUDED, SUCCESS

11 PRs on the target repo in one day: 5 bug fixes (#5–#9), 2 test harnesses
(#10 vitest, #11 Playwright+mock-SignalK, both MERGED), plus stacked
regression tests. Question verdicts:

- **Q1/Q2 (token costs): UNANSWERED** — subagent usage isn't exposed to the
  orchestrating session. Only open item; carries to experiment 1b along with
  Q5. Wall times captured throughout.
- **Q3 (yield/quality): emphatic yes** — 5/5 findings survived human triage,
  zero rejects, all shipped. All clustered in one recent feature commit:
  diff-focused hunting validated.
- **Q4 (rung calibration): the ladder works and rungs are MUTABLE** — shipped
  2× rung 1 + 3× rung 3, then harness work upgraded every rung-3 PR to
  rung ≤2 post-hoc. New insight: test tier follows the CODE'S SHAPE (pure
  logic → extract+unit; cross-boundary → test both sides; UI-glue → E2E at
  protocol level), not the bug's severity.
- **Q5 (cap behavior): untested** (no caps on subagents) → 1b. Adjacent
  lesson banked: a worker dying mid-task (context exhaustion) was cleanly
  resumed from its uncommitted workspace + digested handoff notes — the
  resumable-worker pattern the server needs for window-reset preemption.
- **Q6 (schema v0): held up.** Additions needed: `verdict` (+reason),
  `evidence_rung_achieved` (vs planned), `pr_url`, `status`
  (queued/fixed/shipped/merged). `introduced_by` earned its place.

Process discoveries beyond the questions: triage verdicts flow BOTH ways
(human promoted a queue note to a PR); validation can narrow a finding's
blast radius rather than kill it (#5 → geo-mode-scoped, still merged-worthy);
harness PRs are their own deliverable and upgrade whole bug classes at once.

## Explicitly out of scope

- Scheduler, window chaining, UI, suppression DB, multi-repo, multi-strategy.
- Tuning caps mid-run: if a cap hits, RECORD what state the job died in
  (Q5 data) — don't raise and rerun. Rerun with a new cap is experiment 1b.
