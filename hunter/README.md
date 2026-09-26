# hunter — Idle-Token Bug Hunter

> The daemon is the Rust binary in `hunter-rs/`. This directory is its
> root: local `config.json`, `data/`, `playbooks/`, generated `ui/`, and
> `ui-svelte/`.

Self-hosted [detail.dev](https://detail.dev)-style code analysis pipeline
funded by spare Claude-subscription capacity. Register a repo; hunter finds
bugs, test gaps, outdated dependencies, mechanical refactors, and
modernization opportunities using headless `omp -p` workers; you triage
findings in a local web UI; fix workers ship draft PRs and follow up on
review feedback autonomously; a harvest pass reviews each merged PR's full
lifetime for genuine deferred follow-up work. Runs as a daemon that idles at
zero token cost and only spends against a budget it never overshoots.

## Contents

- [Quick start](#quick-start)
- [How it works](#how-it-works)
  - [Finding types](#finding-types)
  - [Finding lifecycle](#finding-lifecycle)
  - [Scheduling loop](#scheduling-loop)
  - [Budget policy](#budget-policy)
  - [PR follow-up loop](#pr-follow-up-loop)
- [Web UI](#web-ui)
- [API](#api)
- [Architecture](#architecture)
- [Data model](#data-model)
- [Configuration](#configuration)
- [Development](#development)
- [Running as a service](#running-as-a-service)

## Quick start

Requires a Rust toolchain and Node/npm (the daemon binary and the UI
bundle), `omp` on `PATH` (the headless worker CLI this project is built
around), `git`, and the `gh` CLI (or `glab` for GitLab repos)
authenticated for whatever repos you register.

```sh
(cd hunter/ui-svelte && npm ci)           # once per checkout, and after a dependency change
cd hunter-rs
just build                                # UI bundle (ui-svelte/ -> hunter/ui/) + release binary
./target/release/hunter --root ../hunter  # run forever: UI (:8377) + scheduler loop
```

Then open `http://localhost:8377`, go to **Repos**, and add a repository
(name, git URL, default branch). Everything else — registration, triage
verdicts, notes, pausing repos, manual cycle/recheck triggers — happens in
the UI; the binary itself takes only `--root` and `--port`
(hunter-rs/src/main.rs:14-15).

## How it works

### Finding types

| Type | What it looks for | Prompt |
|---|---|---|
| `bug` | Latent bugs — boundary, error-path, race, contract-drift, leak, logic | `playbooks/hunt.md` |
| `test_gap` | Missing test coverage for real code paths | `playbooks/test_gap.md` |
| `dep_update` | Outdated dependencies, with changelog/risk assessment | `playbooks/dep_update.md` |
| `refactor` | Conservative, safe, mechanical refactoring opportunities | `playbooks/refactor.md` |
| `modernization` | SOTA-drift — deprecated deps, language-feature gaps, format/protocol shifts, CI/CD gaps, platform EOL | `playbooks/modernization.md` |

All five share one `findings` table (see [Data model](#data-model)) and one
triage/fix pipeline; the UI's type filter and colored badges are the only
place the distinction usually matters day to day.

### Finding lifecycle

```
new ──recheck──> new / wontfix / rejected
 │
 ├─ queue for fix ─> queued ─> fixing ─> pr_open ─┬─> merged
 │                      ↑           │              └─> rejected (closed unmerged)
 │                      └───────────┘ (requeued on a
 │                        recoverable failure, up to
 │                        a bounded same-reason streak
 │                        before giving up)
 │
 └─ triage verdict ─> wontfix / rejected / note
```

- **`new`** — freshly ingested, awaiting triage (or a `bug` fresh from a
  hunt; other types can also be queued directly for `apply_*` playbooks).
- **`queued` → `fixing` → `pr_open`** — a fix/apply worker checks out a
  fresh worktree, verifies the finding against current code (never trusts
  its own prior analysis), and either ships a draft PR, declines
  (`NOT-A-BUG.md`/`DECLINED.md`/`BLOCKED.md`), or gets requeued. `fixing` is
  structurally impossible to strand: a context-manager guard resets it to
  `queued` on ANY exit path, including an unhandled exception or a process
  kill.
- **`pr_open`** — tracked via `sync_prs` (free — `gh pr view`, no worker)
  every cycle: merged → `merged`; closed unmerged → `rejected` (feeds the
  suppression corpus below); new comments/reviews, `CHANGES_REQUESTED`,
  merge conflicts, or failing checks flag it `needs_attention` for an
  **engage** worker.
- **`merged`** → queued for a one-time **harvest** pass reviewing the PR's
  complete lifetime (not just its open-time snapshot) for genuine deferred
  follow-up work, filed as new findings via `FOLLOW-UPS.json`.
- **`rejected` / `wontfix`** (with a required reason) become the
  **suppression corpus**: injected into every future hunt/analysis prompt
  for that repo+type so the same already-decided issue isn't re-proposed.
- **`note`** — informational, no action expected.
- **Recheck** (UI button, human-triggered only) re-evaluates a `new`
  finding against the *current* codebase with an adversarially skeptical
  second opinion: `confirmed` (refreshes the analysis, stays `new`),
  `stale` (→ `wontfix`), `invalid` (→ `rejected`).

### Scheduling loop

One cycle picks exactly one unit of work, in strict priority order
(`scheduler.pick_next` — the single place this is expressed; the UI's
"what's next" preview calls the same function, so it can never drift from
what actually runs):

1. Any finding with an active **budget override** (any category) — human
   escape hatch, jumps the whole queue.
2. **Engage** the flagged PR with the oldest-outstanding attention reason.
3. **Harvest** the oldest merged PR still pending follow-up review.
4. **Recheck** the oldest finding stuck in `rechecking`.
5. **Fix** the oldest queued finding.
6. **Resume** the newest suspended attempt that can still be continued.
   A worker killed for running out of window headroom (and only that:
   never a wallclock overrun, which is the runaway signature) keeps its
   session and comes back as `suspended` rather than `killed`, and the
   next cycle hands omp that exact session instead of starting cold.
   Ranked here because a suspension has already been paid for — it
   outranks *starting* new background work, never a human waiting on a
   PR. Restarting costs a flat ~37k-token session floor and then redoes
   the work; continuing costs re-caching the context the worker was
   already carrying.
7. Otherwise, **hunt/test_gap/dep_update/refactor** — whichever is most
   overdue for the least-recently-scanned enabled repo (a never-cloned repo
   always hunts first). **Modernization** joins this rotation too, but only
   once `modernization.intervalDays` (config, default 30) has passed since it last ran for that
   repo — a periodic strategic check, not a tight-loop scan competing for
   every cycle.

Each of 1-5 picks a (finding, kind). When that finding has a suspended
attempt of that same kind that can still be continued, the tier continues
it, at the tier's own position, instead of starting the work fresh.

A repeatedly-failing item (same failure reason, consecutive attempts) gives
up after a bounded streak rather than looping forever — this applies
uniformly to fix retries, recheck retries, and harvest retries. A resume
chain has the same kind of ceiling, on either of two counts: once it has
made four attempts, or once — after at least one resume — everything it
spent *besides its single largest attempt* is past three times what that
kind of job typically costs. Whichever fires, the suspension is marked
`failed` (reason `give-up`) instead of being continued again. A chain
that has never been resumed is never retired: one attempt's spend is by
definition at least its own cap, and the largest attempt is discounted
because one enormous attempt says the job is big, not that the chain is
stuck.

Two more things end a suspension, because after either one it can never
usefully be continued, and a suspension nobody ends stays `suspended`
forever:

- **Superseded.** Starting the same work fresh — same finding and kind
  for engage/harvest/recheck/fix, same repo and kind for hunt and the
  analysis scans — marks the suspended attempt `killed` (reason
  `superseded`) in the same transaction that creates the fresh job,
  whichever path started it.
- **Working directory gone.** The transcript describes files in the
  clone (hunt, recheck, analysis scans) or the per-finding worktree
  (fix, engage, harvest). If that directory no longer exists the
  suspension is marked `killed` (reason `workdir-gone`) when selection
  reaches it, and selection moves on to the next candidate the same
  cycle.

### Budget policy

`backends/omp_scavenge` reads omp's local usage mirror
(`~/.omp/agent/agent.db:usage_history`) and gates every job against two
linear ramps, never letting spend get ahead of either:

- **7-day ramp**: allowed fraction = elapsed fraction of the week. Spreads
  spending evenly so a burst early in the week doesn't starve the rest of
  it. Scavenging stops entirely once the interactive-use reserve line is
  crossed (default 75%) — hunter never eats into headroom you need for your
  own interactive sessions.
- **5-hour ramp**: zero for a headroom period (default 30 min, tunable via
  `HEADROOM_MS` in `hunter-rs/src/backends/omp_scavenge/capacity.rs`) so a freshly-opened window is never
  immediately claimed, then ramps 0→1 over what's left. Unspent capacity at
  reset is wasted — there's no rollover.
- **In-flight accounting**: a running job's *anticipated* cost is reserved
  before the next decision, so concurrent/rapid cycles can't overshoot
  either ramp. It is an estimate from history, not the job's granted cap:
  a cold-cache first call arrives as one atomic, uninterruptible LLM call
  that has been observed spending 2-4x what was reserved for it. The
  history is the 20 most recent *completed* jobs of that kind, not all of
  it — a window ages out both rows written under accounting bugs that
  have since been fixed and repos that have since changed size.
- **Start efficiency**: loading context is pure overhead, so no attempt
  starts unless the window can fund at least as much work as the context
  it must load (`MIN_START_EFFICIENCY` = 0.5; never less than 25k of
  work). A resume re-sends its whole transcript (measured: re-cache cost
  equals the context at suspension), so it reserves that context plus
  the larger of what is left of a typical job and that minimum — a
  100k-token transcript reserves at least 200k. With a flat 25k on top it
  would reserve 125k, and an attempt granted 125k spends 80% of it
  re-sending the transcript and 20% working. A cold start reserves the
  larger of its history estimate and 40k (a 15k system-prompt floor plus
  25k of work), which also stops a collapsed estimate from starting jobs
  that die on their second call. A larger reservation does not shrink
  the granted cap — the cap is computed without the job's own
  reservation — it makes the gate refuse until the window can fund both.
- A granted job's token cap is the ramp's remaining headroom, verbatim —
  there is no separate configured per-kind cap. Workers are metered
  **externally**: the harness tails the worker's live session JSONL and
  SIGTERMs the process group at that cap, never depending on the worker
  cooperating with its own limit. A grant with no headroom figure carries
  no token bound, and `maxWallS` alone stops it.
- Stale or missing usage data → conservative denial, never an optimistic
  guess.

### PR follow-up loop

`sync_prs` runs first in every cycle (free — no worker) and refreshes every
open PR's state. Suppression is **state-based, not time-based**: an engage
reply that changes nothing (no commits pushed) records a fingerprint of
the *current static problem* (review decision, merge conflict, and
*which* checks are failing); `sync_prs` won't re-flag that exact static
situation again no matter how long the daemon then runs unattended — only
a genuine change (a different check starts failing, the review state
changes, or a human pushes new code, tracked by head commit SHA so a
same-looking situation after a real push is never mistaken for
"unchanged") clears the suppression. This replaced an earlier flat
time-based backoff, which would have kept re-poking an unfixable problem
every N minutes forever on a long-running unattended daemon.

## Web UI

Served at `http://localhost:8377` (configurable). Left-nav pages:

| Page | Shows |
|---|---|
| **Status** | Budget bars (used + available, per window), what the scheduler is doing right now / why it isn't, and the recent event log |
| **Inbox** | New findings awaiting triage, with per-type filters |
| **Kanban / Pipeline** | Findings in flight (queued → fixing → pr\_open), suppressed (rejected/wontfix) and informational (note) findings |
| **All Findings** | Every finding, filterable by repo/type/status/severity, with full detail (jobs, PR state, timeline) on expand |
| **Repos** | Add/remove/pause repos; per-repo notes (free-text context injected into every prompt for that repo — coding conventions, known false positives, anything worth a hunter remembering across runs) |
| **Stats** | Aggregate totals by kind and by finding type |
| **Log** | Full job and event history |

## API

JSON routes — the UI's only client, but usable directly. Served by axum
(`router` in hunter-rs/src/server.rs).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/summary` | Budget windows, counts, activity status, "what's next" preview |
| GET | `/api/findings` | List findings across all types (`status`, `repo`, `severity`, `type` query params) |
| GET | `/api/finding` | One finding's full job history + PR state |
| GET | `/api/jobs` | Recent jobs (last 50) |
| GET | `/api/repos` | List repos |
| GET | `/api/repo/notes` | One repo's notes |
| GET | `/api/events` | Recent event log |
| GET | `/api/stats` | Aggregate stats |
| POST | `/api/verdict` | Set a finding's triage status (queue for fix, wontfix, reject, note) |
| POST | `/api/cycle` | Trigger one scheduler cycle immediately |
| POST | `/api/scheduler` | Pause or resume automatic scheduler cycles (`{"paused": boolean}`); a running job is allowed to finish |
| POST | `/api/recheck` | Queue a finding for recheck |
| POST | `/api/unqueue` | Pull a finding back out of the fix queue |
| POST | `/api/override` | Set/clear a finding's budget override (`once` \| `exempt`) |
| POST | `/api/repo` | Update a repo (pause/resume, etc.) |
| POST | `/api/repos` | Add a repo |
| POST | `/api/repo/delete` | Remove a repo |
| POST | `/api/repo/notes` | Append a note to a repo |

## Architecture

```
hunter-rs/src/
├── main.rs         CLI: `--root`, `--port`
├── daemon.rs       UI server + scheduler loop + usage prober, lockfile
├── config.rs       config.json loading
├── domain.rs       status/kind enums
├── types.rs        API response + row types (the UI contract)
├── store.rs        SQLite access layer; migrations in hunter-rs/migrations/
├── backend.rs      Backend protocol (decide / run / status)
├── backends/omp_scavenge/
│                   budget policy (§ Budget policy), `omp -p` harness that
│                   meters + kills at cap
├── forge.rs        GitHub/GitLab abstraction (gh/glab CLI wrappers)
├── ingest.rs       validates + dedupes a worker's findings.json into the store
├── playbooks.rs    renders playbooks/*.md templates into worker prompts
├── dep_scan.rs     Renovate-based dependency scan (zero tokens)
├── scheduler.rs    one cycle = one job; run_hunt/run_fix/run_engage/
│                   run_harvest/run_recheck/pick_next/run_cycle
├── server.rs       axum JSON API + serves ui/
└── util.rs         shared subprocess helpers

hunter/
├── playbooks/      worker prompt templates (one per job kind, see table above)
├── ui/             generated Vite bundle (ignored; built by `just build` in hunter-rs/)
├── ui-svelte/      tracked Svelte source
└── config.json     runtime config (§ Configuration)
```

Each analysis/action kind follows the same shape: build a prompt from a
`playbooks/*.md` template, run it through the backend under a
budget-decided cap, then either ingest structured output (`findings.json`,
`FOLLOW-UPS.json`) or interpret marker files the worker wrote
(`NOT-A-BUG.md`, `PR-DESCRIPTION.md`, `WITHDRAW.md`, ...).

## Data model

SQLite (`data/hunter.db`, WAL mode). Tables (see `hunter-rs/migrations/`
for the schema — most columns carry comments explaining *why*, not just *what*):

- **`repos`** — registered repositories, hunt watermarks, enabled/paused.
- **`findings`** — one row per finding across all five types (type
  discriminator column), status, common fields, plus per-type nullable
  columns (bug\_class, dep\_update ecosystem/package/versions, test\_gap
  missing\_tests, refactor smell\_type, modernization\_class), and retry
  streak tracking (fix/recheck) for the give-up mechanism.
- **`jobs`** — one row per worker invocation: kind, cap/actual tokens,
  exit/kill reason, timing, and `resumed_from` (the suspended attempt this
  one continues). States are `queued | running | done | failed | killed |
  suspended | denied`; `suspended` is a pause with a session file to
  continue, everything else killed is terminal. The audit trail for "what
  did hunter actually spend, and on what."
- **`pr_state`** — one row per finding with an open/merged PR: forge state,
  attention fingerprint + suppression marker (state-based, see § PR
  follow-up loop), harvest tracking.
- **`window_log`** — mirror of budget observations at decision time.
- **`calibration_samples`** — empirical tokens-spent → used-fraction-moved
  correlation, informational only (estimates real remaining capacity;
  never gates a decision).
- **`events`** — append-only log backing the Log/Status pages.
- **`scheduler_state`** — single-row snapshot of the daemon's own current
  reasoning, purely informational.

## Configuration

`config.json`:

```jsonc
{
  "workRoot": "data",           // repo clones, worktrees, job output
  "dbPath": "data/hunter.db",
  "ompBin": "omp",
  "hunt":  { "maxWallS": 1800, "maxFindings": 8 },
  "fix":   { "maxWallS": 2700 },
  "budget": { "deny5hAbove": 0.85, "staleAfterS": 300 },
  "serve": { "port": 8377 },
  "models": { "default": "opus", "smol": "sonnet", "hunt": null, "fix": null }
}
```

- `hunt`/`fix` settings apply to their whole job family (hunt also covers
  test\_gap/dep\_update/refactor/modernization/recheck; fix also covers
  engage/harvest/apply\_\*).
- `budget.deny5hAbove` / `staleAfterS` — see § Budget policy.
- `models` — fuzzy names, anything omp's `--model` flag accepts. `default`
  applies to all workers; `smol` is for lightweight helper tasks; `hunt`/
  `fix` override per job family; `null` inherits omp's own configured
  default. Anthropic tracks per-model-class weekly limits
  (`anthropic:7d:<class>`), so splitting hunt and fix across model classes
  taps two separate budgets.

## Development

Rust gates live in `hunter-rs/justfile` (`just lint` and `just test` run fmt,
clippy, `check-sql.sh` and nextest — the same commands CI runs). The
frontend lives in `ui-svelte/`; Vite writes the bundle into the generated
`ui/` (`ui-svelte/vite.config.ts:15`), which `just build` in `hunter-rs/`
does alongside the release binary. Frontend checks: `npx eslint src/`,
`npm run check`, `npx vitest run` in `ui-svelte/`.

Conventions: strict TypeScript on the frontend with hand-rolled structural
validation at the network boundary (`ui-svelte/src/lib/validate.ts`, which
checks exactly what the components dereference unconditionally), so a
shape drift between daemon and UI fails loudly instead of silently
rendering garbage.

## Running as a service

The systemd user unit is checked in at `hunter.service`. Its `ExecStart`
runs the Rust binary directly — `<project-root>/hunter-rs/target/release/hunter
--root <project-root>/hunter`, with `WorkingDirectory=<project-root>/hunter-rs`
(hunter.service:6-7) — so `<project-root>` is the repository root, not this
directory. The daemon never builds the UI: it reads `hunter/ui/` from disk
at request time, so a binary built without the bundle serves a working API
behind a 404 page. `just build` builds both halves.

```sh
(cd hunter/ui-svelte && npm ci)                            # frontend deps; `just build` does not install them
cd hunter-rs && just build && cd ..                        # release binary + ui/ bundle
sed -i "s|<project-root>|$(pwd)|g" hunter/hunter.service   # repo root, not hunter/; `g`: ExecStart has two
cp hunter/hunter.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now hunter.service
loginctl enable-linger $USER   # keep it running after you log out
```

After a rebuild, `systemctl --user restart hunter.service`; `just build`
deliberately does not reach into the service manager itself.

It idles at zero token cost between cycles and wakes on a smart-sleep
policy: right after a job with more queued work → drain immediately;
budget denied → short retry if the ramp is actively rising, otherwise sleep
to the window reset; genuinely idle → 15 minutes. PR-comment polling is
decoupled from the token-budget backoff loop, so a long budget-driven sleep
never delays noticing new PR feedback. Every wake re-checks the budget gate
before spending anything.

