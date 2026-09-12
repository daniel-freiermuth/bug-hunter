# Future — project path, the Rust refactor, and open design questions

Living document. Captures the project's direction, the 2026-09-06
architecture decisions, and ideas/trade-offs too big for a single session.
The big refactor (§3–§6) starts once the plan here is judged good, not
before. Smaller items (§8) proceed independently when they don't conflict
with the port.

---

## 1. Path of the project

Where we came from, where we are, where we're going. Each phase has an exit
criterion; we don't start a phase before the prior one's criterion is met.

### Phase 0 — Experiments (done)
Validated the core economics and mechanics: diff-focused hunting finds real,
human-triage-surviving bugs (exp1: 5/5); token costs are measurable and
cappable externally (exp1b: ~60k/fix, ~150k/hunt, ~32k session floor); window
state is readable from omp's local mirror (exp2). Evidence: `../EXPERIMENT-*.md`.

### Phase 1 — Single-user hunter v1 (current, live)
Python 3.14 stdlib-only daemon: scheduler + `Backend` facade (budget gate +
externally-metered `omp -p` workers, omp specifics confined to
`backends/omp_scavenge/`) + SQLite + triage UI on :8377. 317 tests green as
of 2026-09-06. Runs as a systemd user service. It works; it earned the next
phase.

### Phase 2 — Rust port, feature parity (next)
Port the daemon to Rust, single-user, same behavior, same DB file, same
playbooks. Exit criterion: the Rust binary runs the full cycle (sync → engage
→ fix → hunt) against the live `data/hunter.db` for a week with no regression
vs the Python daemon; Python daemon is then retired.

### Phase 3 — Service-ification
Auth (GitHub OAuth), tenancy (user ownership everywhere), per-user Claude
accounts + budgets, sandboxed workers, per-account scheduler queues. Exit
criterion: a second real user (not the operator) onboards themselves, connects
a repo + subscription, and gets a merged fix PR without operator intervention.

### Phase 4 — Open doors (invite-only)
Small invited group. Webhooks replace polling where volume warrants.
Observability, backups, abuse limits. Grow the job taxonomy (test-gap,
dep-update, refactor hunts already exist; reviews of *incoming third-party*
PRs join here).

---

## 2. Future user stories

The product target: a hosted web service. "Operator" = whoever runs the
instance; "user" = anyone with an account.

### Identity & onboarding
- As a user, I sign in with GitHub (OAuth). No separate password system.
- As a user, I connect repos via a GitHub App installation — the service gets
  scoped, revocable, per-repo credentials. Never a personal global token.
- As a user, I connect my own Claude subscription (BYO tokens — see §5.1) or
  bring my own API key (BYOK — see §8.3). I can see my usage and disconnect
  at any time.

### Hunting & triage (exists single-user; must survive tenancy)
- As a user, I register a repo; hunts run on idle budget from *my* account.
- As a user, I triage findings in the UI (queue / reject / wontfix / note),
  and my rejection reasons become my repo's suppression corpus.
- As a user, I can open a full finding detail page: complete evidence/plan,
  job history with token costs, PR timeline, recheck results, related
  findings (§8.5).
- As a user, I can trigger a manual cycle or an adversarial recheck.
- As a user, I tune per-repo and per-kind scan intervals ("hunt this repo
  every 6h, dep-update weekly") (§8.4).

### Fix & PR loop (exists single-user; must survive tenancy)
- As a user, queued findings become draft PRs authored via my GitHub App
  installation; PR feedback is noticed and engaged automatically.
- As a user, I see per-PR state (checks, reviews, conflicts) in the UI.

### PR reviews (new)
- As a user, I enable reviews on a repo: incoming third-party PRs get an
  automated review (evidence-first: comments must cite code, never vibes).
- As a user, I set the trigger: all PRs, or on-mention, or label-gated.

### Safety & trust
- As a user, my repo's code never executes with access to another tenant's
  data, tokens, or the host (sandboxed workers, §5.3).
- As a user, my Anthropic token is encrypted at rest and never leaves the
  worker boundary for another tenant's job.
- As an operator, a worker can never push to a default branch (existing
  invariant, kept infra-level).

### Operations
- As an operator/user, I see budget state, cost-per-finding/repo, burn-rate
  charts, job history, and failure modes (§8.2).
- As an operator, the whole service is one binary + one SQLite file + Caddy;
  backup = Litestream-style replication of the DB.

---

## 3. The decision: pivot to Rust

**Status:** decided 2026-09-06, planning here; build starts when this doc is
judged a good plan.

Not a bug-prevention fantasy — the recorded bug history (prompt-escaping,
GitHub timestamp skew, force-push semantics, budget policy) is
policy/integration bugs a type system doesn't catch. The pivot rests on four
arguments that are true anyway:

1. **Tenancy safety is type-shaped.** The Python core flows through
   `Row = dict[str, Any]` with stringly access (~80+ sites in scheduler.py
   alone). Multi-tenancy is exactly where that style fails: one forgotten
   `WHERE user_id = ?` is a cross-tenant leak, and dict-rows make it silent.
   sqlx compile-time-checked queries + a `UserId` newtype make it structural.
2. **AI-maintained codebases inherit the language's *median* corpus register.**
   This repo is AI-coded, and it defaulted to Python's median idiom (dict
   shuffling, `.get()` chains) — nobody chose that. Python's training corpus
   is huge, unfiltered, and self-reinforcing (bad Python runs, gets committed,
   becomes corpus). Rust's corpus was pre-filtered by the compiler. More
   important: the compiler is an in-loop verifier — AI mistakes bounce off
   `cargo check` in the same turn at near-zero cost instead of surfacing at
   runtime if a test happens to cover them. At hundreds of AI edits, you get
   the language's median output at scale; pick the language whose median is
   acceptable.
3. **The domain fits.** Finding statuses are a state machine begging for
   exhaustive `match`; the `Backend` protocol's `Outlook` (paired
   Granted/Denied verdicts) is a sum type already straining against Python;
   forge responses become serde structs; "run_cycle never raises" is what
   `Result` is. A long-running daemon holding other people's OAuth tokens is
   where one static binary, no runtime, memory-safe stops being aesthetics.
4. **Timing is now-or-never.** This is the smallest the codebase will ever
   be, and the webservice plan rewrites most of server.py regardless. Plus:
   side project with learning as an explicit goal, and the operator is
   productive in Rust.

**Costs, accepted knowingly:** worker plumbing (subprocess spawn, JSONL
tailing, SIGTERM trees) is more verbose; iteration on scheduling/prompt logic
is slower; borrow-checker friction in shared-state spots. Counterweights:
models hallucinate more in exotic Rust — stay in the best-represented corner
of the ecosystem (axum/tokio/sqlx/serde, boring mainstream crates only). And
the compiler catches type bugs, not policy bugs: playbook/policy correctness
still needs tests and review in any language.

**Rejected alternatives, for the record:**
- *Tighten Python instead* (strict pyright, dataclasses at boundaries): viable
  for a deadline work project, but it fights the corpus prior with per-session
  instructions, and instructions decay; compilers don't.
- *Go / TypeScript:* no argument beat Rust for this operator's goals; the
  concurrency needs are trivial in any of them.
- *Moving off SQLite:* rejected. Write rates are tiny; WAL serves hundreds of
  triage users; single-writer matches the scheduler model; migration is a
  file copy. Revisit only with real multi-node pressure (unlikely ever).

---

## 4. Target stack

| Layer | Choice | Notes |
|---|---|---|
| Language | Rust | see §3 |
| HTTP | axum + tower | tower-sessions for session auth |
| Async runtime | tokio | per-account queues are tasks, not threads |
| DB | SQLite via sqlx | same file, same schema lineage; compile-time-checked queries |
| Serialization | serde | typed structs for worker JSONL, forge responses, config |
| Auth | GitHub OAuth (`oauth2` crate) | login = the identity users already have here |
| Repo access | GitHub App installation tokens | kills the global `gh` credential model |
| TLS / edge | Caddy reverse proxy | the binary stays plain HTTP on localhost |
| UI v1 | Svelte SPA against a clean JSON API | novelty budget goes to the backend; one unknown at a time |
| UI styling | Tailwind, adopted *with* the Svelte rewrite | supersedes the earlier plan to migrate the vanilla-TS UI to Tailwind first (§8.1) |
| UI later | Leptos experiment allowed | affordable *because* the JSON API boundary makes the UI swappable in a weekend |
| Sandbox | rootless podman per job | §5.3 |
| Backup | Litestream-style SQLite replication | Phase 4 |

**What ports unchanged (language-neutral assets):** the SQLite schema
(`schema.sql` lineage), `playbooks/*.md`, `config.json` shape, the omp worker
contract (headless `omp -p`, session-JSONL metering, SIGTERM at cap,
kill-safe incremental output), the systemd unit approach, and the 317 Python
tests — which are the behavioral spec, ported module-by-module alongside each
component.

---

## 5. The hard problems (none are framework-shaped)

### 5.1 Funding model: BYO subscription
Each user connects their own Claude subscription; the service schedules
against *their* windows. Consequences:
- Per-user omp homes (`OMP_HOME=data/users/<id>/omp`) with injected OAuth
  credentials; per-user usage probing; per-user budget gates — i.e. one
  `Backend` instance per connected account.
- We store users' Anthropic OAuth tokens server-side → encrypt at rest,
  minimize exposure surface, audit access.
- **Flagged risk:** automating consumer subscriptions on users' behalf in a
  hosted product is ToS-gray with Anthropic. Decide consciously before
  inviting strangers; invite-only keeps this a group of consenting adults.
- Operator-funded alternative was considered and argued against: it inherits
  metering/fairness/abuse problems and breaks the per-account budget model.
- BYOK (API key, dollar budgets) is the complementary funding model and the
  second `Backend` implementation (§8.3).

### 5.2 Scheduler: parallelism, per-repo then per-account
Two layers, one design:
- **Single-account (portable today):** one job per repo concurrently instead
  of one global job. The per-repo sequential invariant stays (git worktree
  safety). Needs per-repo locking instead of the global `_cycle_lock`; the
  budget gate already accounts for in-flight jobs via
  `SpendLedger.running_estimate()`. The `Backend` protocol supports this —
  nothing in `decide()`/`run()` assumes single-threaded dispatch; only the
  daemon loop and the Store's job-state management assume one-at-a-time.
- **Multi-account (Phase 3):** one logical queue per connected Claude
  account, a global concurrency cap on top, `decide()` evaluated per-account.
  The one-job-at-a-time rule exists because parallel fan-out self-DoSed *one
  account's* 5h window (observed live) — the constraint is per-account, so N
  users run N concurrent workers safely.

In the Rust port, tokio tasks make both layers natural; design the scheduler
loop for per-repo tasks from the start, even if v1 caps global concurrency
at 1.

### 5.3 Sandboxing (mandatory before any second tenant)
Today a hunted repo's build script runs with the operator's full credentials.
Required shape:
- `Backend::run()` stays the single spawn seam; wrap the argv in rootless
  podman: mount in the job worktree + that user's omp home, nothing else.
- Session-JSONL metering currently reads the host `~/.omp/agent/sessions` —
  the container moves that path; the meter follows via volume mount or
  in-container path mapping.
- The real work is credential scoping, not container tech: per-repo GitHub App
  installation tokens instead of any ambient identity.
- Sandboxing is worth doing while still single-user (repo code currently sees
  operator credentials) — it is Phase 3's first item, not its last.

**Stepping stone (single-user, pre-container): bwrap.**  bubblewrap is
rootless, already available on most Linux systems, and needs zero infra.
The harness wraps the `Popen` call:
```
bwrap \
  --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib \
  --ro-bind /etc /etc --symlink usr/lib64 /lib64 \
  --proc /proc --dev /dev --tmpfs /tmp \
  --bind data/envs/$SLUG /home/hunter \
  --bind $WORKTREE $WORKTREE \
  --setenv HOME /home/hunter \
  -- omp -p "..."
```
This gives the worker full access to the host's toolchain (gradle, cargo,
node, python — all in `/usr/bin`) while hiding `$HOME`, `~/.ssh`,
`~/.config/gh`, and other repos. Blast radius of a compromised worker =
one project's build cache. No container images, no per-repo Dockerfiles.

**Per-project persistent environments.**  Each repo gets a persistent home
directory (`data/envs/<repo-slug>/`) that persists across runs. Workers can
install deps, configure tools, prime caches — whatever the project needs.
First run is slow (bootstrapping), subsequent runs reuse the cached env.

```
data/envs/
  glosdalen/         ← persistent $HOME for glosdalen workers
    .gradle/
    .m2/
    .local/
  signalk-pose/      ← separate env, separate project
    .cargo/
    node_modules/
```

This solves the container tool-availability problem without fat base images:
the host's tools are bind-mounted read-only, the project's deps live in
its own persistent env. A worker can `npm install` on first run; next run
it's already there. To reset a broken env: `rm -rf data/envs/<slug>/`.

Environment setup can be:
1. **Automatic:** the existing playbooks already tell the worker to build
   and test. If deps are missing, the worker installs them. The env
   persists, so next run they're already there.
2. **Explicit:** per-repo setup script or a first-run playbook triggered
   when `data/envs/<slug>/` is empty.

Option 1 matches "add a repo URL and go" — no per-repo config needed.

**Credential exposure audit (current state):**
The worker subprocess inherits the full host environment. Specifically
exposed but NOT needed by the worker:
- `~/.ssh/*` — the scheduler pushes, not the worker
- `~/.config/gh/*` — the scheduler calls `gh`, not the worker
- `~/.omp/agent/*` — the harness reads session JSONLs externally
- Other repos' worktrees in `data/wt/` and `data/repos/`
- All env vars, full network, full filesystem

The bwrap approach hides all of these. The podman endgame adds network
isolation (deny all except git+LLM endpoints) and per-repo credential
injection via GitHub App installation tokens.

**System-package installation under bwrap — the `/usr` tension.**
bwrap bind-mounts `/usr` read-only from the host, so the worker gets
the host's toolchains (JDK, Node, Python, Rust, etc.) for free. But
`apt install` / `dnf install` / `pacman -S` need root and a writable
`/usr` — neither available in a rootless bwrap sandbox.

Three tiers, in order of complexity:

1. **Don't (covers ~95% of repos).** Most projects don't need system
   packages installed — they need their own build tool's dependency
   manager (`npm install`, `pip install -r`, `./gradlew`, `cargo build`),
   which all install into the worktree or `$HOME` (userspace). The host
   already has the language runtimes; the project-level deps are what
   the persistent env caches. If a repo genuinely needs a system package
   the host doesn't have, install it on the host (single-user scenario)
   — this is the same thing you'd do for your own dev work.

2. **Per-project overlay (rootless overlayfs).** Layer a writable overlay
   on top of the read-only `/usr` via fuse-overlayfs. The worker sees a
   writable `/usr/bin` but writes actually go to
   `data/envs/<slug>/overlay/`. Persists across runs, per-project, host
   untouched. bwrap supports this with `--overlay-src /usr --overlay
   data/envs/<slug>/usr-overlay --tmp-overlay /usr`. More complex but
   gives full `apt install` capability without root or containers.
   Trade-off: the overlay accumulates drift from the host's base — an
   `apt upgrade` on the host doesn't propagate into existing overlays.

3. **Podman with persistent volumes (the endgame).** A rootless container
   with a base image providing the OS + system packages, and a persistent
   named volume for the project home. Workers can `apt install` freely
   inside the container — it's an ephemeral filesystem layer. The project
   home volume persists; the system-package layer is cheap to rebuild from
   the base image. This is heavier but is the multi-tenant endgame anyway.
   The base image can be thin (debian-slim + git + common runtimes) since
   the worker self-provisions the rest.

   Podman fits this well specifically because:
   - **Rootless by default** — runs as your user, no daemon, no privilege
     escalation. Matches bwrap's security posture.
   - **Volumes are just directories** — stored at
     `~/.local/share/containers/storage/volumes/<name>/_data/`. Inspectable,
     removable, backuppable. `podman volume rm hunter-env-glosdalen` resets
     a broken env.
   - **No registry needed** — `hunter-base` can be built locally from a
     Dockerfile or `podman import` from a tarball. No network dependency
     after initial build.

   Concrete shape:
   ```
   # One-time per repo
   podman volume create hunter-env-glosdalen

   # Every worker run
   podman run --rm \
     -v hunter-env-glosdalen:/home/hunter \
     -v $WORKTREE:$WORKTREE \
     --network=none \           # deny all network (scheduler pushes, not worker)
     hunter-base \
     omp -p "..."
   ```

   The volume persists across `--rm` containers. `apt install`, global caches,
   tooling config all survive in `/home/hunter`. The container filesystem itself
   is ephemeral — a corrupted system state is just a re-pull from the base image
   while the project's deps stay cached.

   Shared caches are possible — content-addressed caches (gradle, cargo
   registry, pip wheels) can be mounted read-write across repos:
   ```
   -v hunter-cache-gradle:/home/hunter/.gradle
   -v hunter-cache-cargo:/home/hunter/.cargo/registry
   ```

   omp availability inside the container — options:
   1. Bind-mount the host binary: `-v $(which omp):/usr/bin/omp:ro`
   2. Install it in the base image
   3. Bind-mount `/usr` read-only (same as bwrap — keeps all host tools,
      container only adds writable overlay for `apt install`). Makes podman
      behave like "bwrap with network isolation and system package support."

   Option 3 is appealing for single-user: no base image to maintain, host
   tool updates propagate instantly, and the container's value-add is purely
   isolation + writable system layer. Option 2 is cleaner for multi-tenant
   (the container is self-contained, portable, reproducible).

Recommendation: start with tier 1 (host tools + userspace deps). Move
individual repos to tier 3 (podman) when they genuinely need system
packages the host doesn't have or when multi-tenancy requires full
isolation. Tier 2 (overlay) is a niche middle ground — worth knowing
about but probably not worth the complexity if podman is the endgame.

### 5.4 Harness coupling (the most fragile dependency, in any language)
Session-JSONL format, `agent.db:usage_history` schema, session-file reuse
behavior — a harness update breaks metering and the budget gate silently, and
the failure direction is overspend. The Python side already confines this:
the `Backend` protocol (`backend.py`: `decide()` / `run()` / `keep_fresh()`
+ `SpendLedger`) with all Anthropic/omp vocabulary inside
`backends/omp_scavenge/` (facade + capacity). The port keeps that boundary:
- `Backend` becomes a Rust trait; `omp_scavenge` its first impl. A future
  switch (e.g. Claude Agent SDK with programmatic usage callbacks) or a BYOK
  backend (§8.3) is a module swap.
- Pin the omp version the service uses.
- Startup canary: parse a known-good ledger fixture + assert the
  `usage_history` schema; refuse to run blind.

---

## 6. Port plan (Phase 2, dependency order)

Each step ports the module *and* its test suite; tests are the spec. The Rust
binary and the Python daemon can run side-by-side against the live DB during
transition (SQLite WAL is multi-process) — cut over component-wise, e.g. Rust
`serve` first while Python still schedules.

1. **types** → domain structs: `FindingStatus` etc. as enums with typed
   transitions; `Config` via serde. Kill `Row` — no dict-shaped core.
2. **store** → sqlx against the existing schema; add nullable `user_id`
   ownership columns (backfilled to operator) *during* the port — tenancy
   retrofits get expensive later. `SpendLedger` impl comes with it (the
   thread-local dance in `ThreadLocalLedger` dissolves under sqlx's pool).
3. **backend protocol + omp_scavenge** → `Backend` trait (`decide`/`run`/
   `keep_fresh`), `Outlook`/`Granted`/`Denied` as enums, capacity math
   (ramps, unaccounted-token tracking) ported test-first — it's pure logic
   and spec-rich; proves the pattern. Diagnostics cross as *data*, not HTML
   (settles §8.1's open question as option 2).
4. **runner** → the subtle one: spawn, ledger discovery (session-file reuse
   quirk!), live tailing, SIGTERM-at-cap, kill-safety.
5. **forge** → serde-typed GitHub/GitLab responses; GitHub App tokens replace
   `gh` CLI ambient auth (may land as a Phase 3 follow-up; keep the seam).
6. **scheduler** → port = refactor: the copy-pasted `run_*` skeletons
   (hunt/recheck/test-gap/dep-update/refactor share one ~100-line shape)
   become one generic job executor + per-kind descriptor (playbook builder,
   `JobClass`, model key, output parser). Do NOT transliterate the
   duplication. New job kinds (PR review) become a descriptor entry, not a
   copy. Structure the loop for per-repo parallelism (§5.2) even if capped
   at 1.
7. **server** → axum from scratch (it's being redesigned for auth anyway);
   UI initially unchanged (current vanilla-TS SPA talks to the same JSON
   endpoints), Svelte+Tailwind rewrite follows as its own step.

Invariants that MUST survive the port (all currently enforced and tested):
- Workers never push to a repo's default branch (infra guard, not prompt-only).
- PR-branch history rewrites before merge are normal workflow, not violations.
- Plain `--force` on raw-URL pushes (`--force-with-lease` structurally fails
  without a named remote — twice-learned).
- Fix-queue drains before new hunts; PR feedback sync is never delayed by
  token-budget backoff.
- run_cycle survives anything; the daemon loop never dies to a job error.
- Kill-safe incremental findings output (hunt playbook contract).
- 60s watermark buffer on GitHub timestamp comparisons (±1.6s observed skew).
- Prompt building escapes ALL interpolated content (template injection).
- `Outlook` invariant: prioritized is at least as permissive as normal.

---

## 7. Open decisions

| # | Decision | Status |
|---|---|---|
| 1 | BYO subscription as the funding model | **Leaning yes** (argued §5.1); confirm before Phase 3 code |
| 2 | ToS-gray of hosting others' Anthropic tokens | Open; invite-only mitigates; revisit before strangers |
| 3 | Svelte vs Leptos for the UI rewrite | Svelte for v1 decided; Leptos experiment deferred until JSON API is stable |
| 4 | `gh` CLI → GitHub App: during port (step 5) or Phase 3 | Open; seam kept either way |
| 5 | Webhooks vs polling threshold | Defer to Phase 4; polling fine at current repo counts |
| 6 | Keep omp vs Claude Agent SDK for workers | Keep omp; revisit only if the coupling (§5.4) keeps hurting — trait makes it cheap |
| 7 | Repo layout: new `hunter-rs/` beside `hunter/`, or cargo workspace at root | Decide at scaffold time |
| 8 | Tailwind on the *current* UI before the port | **No** — superseded; adopt Tailwind with the Svelte rewrite (§8.1) |

---

## 8. Nearer-term items (pre-existing living-doc entries, reconciled with the pivot)

### 8.1 UI: Tailwind CSS + `status() -> data, not HTML`
**Status:** superseded / resolved by the port plan.

Previously decided: migrate the hand-rolled ~300-line CSS to Tailwind on the
current vanilla-TS UI. The Rust pivot changes the calculus — migrating the
current UI to Tailwind and then rewriting it in Svelte pays the migration
twice. Tailwind arrives *with* the Svelte rewrite (port step 7); the token
system (spacing/type scale, radii, shadows) maps into `tailwind.config.js`
so the visual identity survives.

The dependent question — backend `status() -> str` (HTML fragment) — is
settled by the port as the previously-identified endgame (option 2): the
backend emits typed data (`BudgetDimension[]`-style), the client renders it.
Rationale unchanged: the HTML fragment couples backend to CSS class names,
can't be validated at the schema boundary (`z.string()`), is the only
non-data-driven panel, and blocks Tailwind's scanner. In the Rust `Backend`
trait, diagnostics cross the boundary as serde data from day one. Until the
port, the Python `status() -> str` stays as-is — no interim work.

### 8.2 Scheduler: job-level cost tracking and reporting
**Status:** idea; lands as a UI story in Phase 3/4 (§2 Operations).

`anticipated_tokens` already tracks historical cost distributions per kind.
Surface it: cost-per-finding, cost-per-repo, burn-rate charts. Data already
in the jobs table (`tokens_new`, `usage_delta`); needs a `/api/costs`
endpoint + a Stats sub-panel. Related: the `estimate_capacity` calibration
(p75 of token/fraction ratios) currently only feeds the "~Nk tok avail"
display; it could replace the hardcoded `200k ≈ 10%` conversion in the
facade, at least for display.

### 8.3 Backend: BYOK / bring-your-own-key
**Status:** open; the `Backend` protocol enables it; becomes a funding-model
pillar in Phase 3 (§5.1).

Most likely second backend: same omp harness (spawn/meter/kill JSONL
pattern), accounting via direct Anthropic API key (usage from the API, not
agent.db) — or OpenRouter/other provider — with a dollar-budget policy
(daily/monthly cap) instead of scavenging ramps. The protocol's genericity
is untested by a second implementation; a BYOK spike validates or breaks the
interface at exactly the joints the Rust trait must get right:
- `decide()` — does `Outlook` generalize beyond ramp-based pacing?
- `run()` — does `JobClass` (HUNT/FIX) make sense for a dollar backend?
- `keep_fresh()` — does it mean anything without probe staleness?
- `SpendLedger` — are 7 methods too many? Too few?

Worth doing as a spike *during* port step 3 (even a stub impl), so the trait
is shaped by two consumers before it hardens.

### 8.4 Scheduler: per-repo and per-kind scan intervals
**Status:** `scan_interval_days` done; extension idea.

Per-repo overrides in the repos table ("scan this repo every 6h, that one
weekly"; config default as floor). Per-kind intervals generalize the existing
`modernization_interval_days` into `scan_intervals: {hunt: 1, dep_update: 7,
...}`. Cheap to include in the Rust store/scheduler port; becomes a user
story in Phase 3 (§2).

### 8.5 UI: finding detail page
**Status:** idea; folded into the Svelte rewrite scope (§2 triage stories).

Full-page detail view: complete evidence/plan text, job history with token
costs, PR timeline, recheck results, related findings in the same
file/class. Data is already served (or trivially servable) by
`/api/finding/<id>`; it's purely a UI page. Don't build it twice — skip on
the vanilla UI, include in the Svelte rewrite.

### 8.6 Process: multi-repo parallelism
**Status:** absorbed into §5.2 (single-account layer). The prior analysis
(per-repo locking, `running_estimate` already in-flight-aware, protocol
already dispatch-agnostic) carries over verbatim as the design basis.
