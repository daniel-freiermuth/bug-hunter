# hunter — Idle-Token Bug Hunter

> **The Python daemon in `hunter/` is no longer what runs.** The service
> executes the Rust binary (`hunter-rs/`), which owns `data/hunter.db` and
> applies its own migrations. This package is retained only as a rollback
> until the Phase 2 exit criterion in `future.md` is met — a week of the
> Rust daemon against the live database with no regression — and is then
> deleted. Until then it is expected to keep working: a rollback nobody
> has kept correct is not a rollback. So fix defects found here — just
> triage them as fallback defects rather than outages, since nothing in
> production executes this code.
>
> Everything else under `hunter/` is live: local `config.json`, `data/`,
> `playbooks/`, generated `ui/`, and `ui-svelte/`. It is the daemon's root
> directory.

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
authenticated for whatever repos you register. Python 3.13+ is needed only
for the rollback daemon in this package.

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

The Python rollback daemon in this package keeps its own two commands, and
serves the same generated `ui/` bundle:

```sh
cd hunter
python3 -m venv .venv                       # must be 3.13+
.venv/bin/python -m pip install -e .        # pydantic, the only runtime dep
.venv/bin/python -m hunter daemon           # UI + scheduler loop
.venv/bin/python -m hunter serve            # UI only, no scheduler (local inspection)
```

One interpreter for the install and the run, for the reasons the rollback
section below spells out: a bare `pip` may belong to a different Python,
PEP 668 distros refuse it outright, and the system `python3` may predate
3.13. It is the same `.venv` the rollback unit points at.

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
6. Otherwise, **hunt/test_gap/dep_update/refactor** — whichever is most
   overdue for the least-recently-scanned enabled repo (a never-cloned repo
   always hunts first). **Modernization** joins this rotation too, but only
   once `modernization.intervalDays` (config, default 30) has passed since it last ran for that
   repo — a periodic strategic check, not a tight-loop scan competing for
   every cycle.

A repeatedly-failing item (same failure reason, consecutive attempts) gives
up after a bounded streak rather than looping forever — this applies
uniformly to fix retries, recheck retries, and harvest retries.

### Budget policy

`budget.py` reads omp's local usage mirror
(`~/.omp/agent/agent.db:usage_history`) and gates every job against two
linear ramps, never letting spend get ahead of either:

- **7-day ramp**: allowed fraction = elapsed fraction of the week. Spreads
  spending evenly so a burst early in the week doesn't starve the rest of
  it. Scavenging stops entirely once the interactive-use reserve line is
  crossed (default 75%) — hunter never eats into headroom you need for your
  own interactive sessions.
- **5-hour ramp**: zero for a headroom period (default 30 min, tunable via
  `HEADROOM_MS` in `budget.py`) so a freshly-opened window is never
  immediately claimed, then ramps 0→1 over what's left. Unspent capacity at
  reset is wasted — there's no rollover.
- **In-flight accounting**: a running job's *anticipated* cost (not its
  nominal cap — cold-cache first calls have been observed running
  2-5x cap_tokens in one atomic, uninterruptible LLM call) is reserved
  before the next decision, so concurrent/rapid cycles can't overshoot
  either ramp.
- Workers are metered **externally**: `runner.py` tails the worker's live
  session JSONL and SIGTERMs the process group at the token cap — it never
  depends on the worker cooperating with its own limit.
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

JSON routes — the UI's only client, but usable directly. Both servers
expose the same set: axum in the Rust daemon (`router` in hunter-rs/src/server.rs),
hand-rolled on `ThreadingHTTPServer` in the Python rollback.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/summary` | Budget windows, counts, activity status, "what's next" preview — validated against a pydantic schema before it ships |
| GET | `/api/findings` | List findings across all types (`status`, `repo`, `severity`, `type` query params) |
| GET | `/api/finding` | One finding's full job history + PR state |
| GET | `/api/jobs` | Recent jobs (last 50) |
| GET | `/api/repos` | List repos |
| GET | `/api/repo/notes` | One repo's notes |
| GET | `/api/events` | Recent event log |
| GET | `/api/stats` | Aggregate stats |
| POST | `/api/verdict` | Set a finding's triage status (queue for fix, wontfix, reject, note) |
| POST | `/api/cycle` | Trigger one scheduler cycle immediately |
| POST | `/api/recheck` | Queue a finding for recheck |
| POST | `/api/unqueue` | Pull a finding back out of the fix queue |
| POST | `/api/override` | Set/clear a finding's budget override (`once` \| `exempt`) |
| POST | `/api/repo` | Update a repo (pause/resume, etc.) |
| POST | `/api/repos` | Add a repo |
| POST | `/api/repo/delete` | Remove a repo |
| POST | `/api/repo/notes` | Append a note to a repo |

## Architecture

```
hunter/
├── __main__.py     entry point -> cli.py
├── cli.py          argument parsing: `daemon` | `serve`
├── types.py        shared dataclasses/TypedDicts, Config, Status/enums
├── store.py        SQLite access layer (schema.sql + migrations)
├── budget.py       two-ramp budget policy (§ Budget policy)
├── runner.py       spawns headless `omp -p`, meters + kills at cap
├── forge.py        GitHub/GitLab abstraction (gh/glab CLI wrappers)
├── ingest.py       validates + dedupes a worker's findings.json into the store
├── playbooks.py    renders playbooks/*.md templates into worker prompts
├── scheduler.py    one cycle = one job; run_hunt/run_fix/run_engage/
│                   run_harvest/run_recheck/pick_next/run_cycle
├── server.py       ThreadingHTTPServer + JSON API + serves ui/
└── util.py         shared subprocess wrapper

playbooks/          worker prompt templates (one per job kind, see table above)
ui/                 generated Vite bundle (ignored; built by `just build` in hunter-rs/)
ui-svelte/          tracked Svelte source
schema.sql           this package's schema; store.py migrates existing DBs. The live
                     database is migrated by hunter-rs/migrations, and the two converge
config.json          runtime config (§ Configuration)
tests/               pytest suite, one file per module/behavior
```

Each analysis/action kind follows the same shape: build a prompt from a
`playbooks/*.md` template, run it through `runner.run_worker` under a
budget-decided cap, then either ingest structured output (`findings.json`,
`FOLLOW-UPS.json`) or interpret marker files the worker wrote
(`NOT-A-BUG.md`, `PR-DESCRIPTION.md`, `WITHDRAW.md`, ...).

## Data model

SQLite (`data/hunter.db`, WAL mode). Tables (see `schema.sql` for full
column docs — most have inline comments explaining *why*, not just *what*):

- **`repos`** — registered repositories, hunt watermarks, enabled/paused.
- **`findings`** — one row per finding across all five types (type
  discriminator column), status, common fields, plus per-type nullable
  columns (bug\_class, dep\_update ecosystem/package/versions, test\_gap
  missing\_tests, refactor smell\_type, modernization\_class), and retry
  streak tracking (fix/recheck) for the give-up mechanism.
- **`jobs`** — one row per worker invocation: kind, cap/actual tokens,
  exit/kill reason, timing. The audit trail for "what did hunter actually
  spend, and on what."
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
  "hunt":  { "capNewTokens": 200000, "maxWallS": 1800, "maxFindings": 8 },
  "fix":   { "capNewTokens": 150000, "maxWallS": 2700 },
  "budget": { "deny5hAbove": 0.85, "staleAfterS": 300 },
  "serve": { "port": 8377 },
  "models": { "default": "opus", "smol": "sonnet", "hunt": null, "fix": null }
}
```

- `hunt`/`fix` caps apply to their whole job family (hunt also covers
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

```sh
just test           # pytest tests/
just test-one NAME  # pytest tests/NAME.py -v
just fmt-check      # ruff format --check
just lint           # ruff check
just typecheck      # mypy --strict
```

Or directly:

```sh
uv run pytest tests/ -q
uv run mypy --strict hunter/
uv run ruff check hunter/ tests/
```

The frontend is not built from here any more: it lives in `ui-svelte/` and
Vite writes the bundle into the generated `ui/` (`ui-svelte/vite.config.ts:15`),
which `just build` in `hunter-rs/` does alongside the release binary. This
package's `build-ui` and `typecheck-ui` recipes still point at
`ui/src/app.ts` and `ui/tsconfig.json`, both deleted in that move, so they
fail — and with them `just check`, `just fix`, `just daemon` and `just
serve`, which depend on them. Use the commands above until the recipes are
repaired.

Conventions: `mypy --strict` on all Python; strict TypeScript on the
frontend with hand-rolled structural validation at the network boundary
(`ui-svelte/src/lib/validate.ts`, which checks exactly what the components
dereference unconditionally) and a pydantic-validated boundary on the
backend for `/api/summary`, so a shape drift between them fails loudly
instead of silently rendering garbage. Tests are colocated
one file per behavior/module under `tests/`, favoring real fixtures (actual
git repos in `tmp_path`, an in-memory-equivalent SQLite `Store`) over deep
mocking.

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

### Rollback: running the Python daemon instead

Kept working on purpose (see the note at the top of this file), so these
steps stay here — but they are the rollback path, not the normal one. Both
daemons share `data/hunter.db` and the same generated `ui/` bundle, and the
Python store migrates whatever database it opens to the same shape
(`tests/test_schema_parity.py` asserts the two agree), so switching is a
unit edit plus a restart:

```sh
systemctl --user stop hunter.service
cd hunter && pip install -e . && cd ..     # pydantic, the only runtime dep
# then in ~/.config/systemd/user/hunter.service:
#   WorkingDirectory=<project-root>/hunter
#   ExecStart=/usr/bin/python3 -m hunter daemon
systemctl --user daemon-reload
systemctl --user start hunter.service
```

Switching back is the same edit in reverse, against the checked-in unit.
