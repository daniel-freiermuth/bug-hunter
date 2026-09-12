# Future — larger refactors and open design questions

Living document. Captures ideas, trade-offs, and decisions-in-progress
that are too big for a single session or need alignment before building.

---

## UI: adopt Tailwind CSS

**Status:** decided, not started

The hand-rolled CSS now has a proper token system (spacing scale, type
scale, radii, shadows), but it's still ~300 lines of manual rules that
grow with every component. Tailwind replaces the rules with utility
classes and brings consistency for free.

Migration path:
- Add `tailwindcss` CLI as a dev dep (single binary, no PostCSS needed)
- `tailwind.config.js` maps the existing CSS variables (colors, spacing,
  type scale) so the visual identity doesn't change
- Incremental migration: new components use utilities, old rules get
  replaced section by section
- Build step: `tailwindcss -i ui/src/input.css -o ui/styles.css`,
  chained with the existing esbuild command in `npm run build`

Open question: backend-rendered HTML (`status() -> str`). Options:
1. Backend emits Tailwind utility classes (needs a safelist in the config
   since the scanner can't see Python string templates)
2. Backend emits structured data, client-side component renders it
   (eliminates the HTML fragment contract entirely)
3. Keep scv-* classes alongside Tailwind (two systems, but isolated)

Option 2 is probably the endgame — it was the direction the design
discussion was heading before we chose the "boring" HTML approach. The
status panel would become a typed JSON response (`BudgetDimension[]` or
similar) with a client-side renderer, and the backend protocol's
`status()` would return data, not markup. This also unblocks Tailwind
fully since there's no more server-generated HTML to scan.


## Backend: status() -> data, not HTML

**Status:** open, depends on Tailwind decision

The current `status() -> str` (HTML fragment) was explicitly chosen as
the boring-but-works option. It gives the backend full rendering freedom
but:
- Couples the backend to CSS class names (even prefixed ones)
- Can't be validated at the zod boundary (just `z.string()`)
- Makes the status panel the only part of the UI that isn't data-driven
- Blocks Tailwind adoption for that panel (scanner can't see Python)

If we adopt Tailwind (above), revisit the earlier design discussion about
a small display-model vocabulary (Gauge + Note widgets) or structured
`BudgetDimension[]` data. The key constraint that killed the semantic
schema was "it won't generalize" — but a presentation vocabulary (gauge
label/fill/marker, not budget semantics) might still work. Or just let
the client-side code own the rendering from a typed data response, which
is what every other panel already does.


## Backend: BYOK / bring-your-own-key

**Status:** open, the refactor enables it

The `Backend` protocol is ready for a second implementation. The most
likely next backend:

- **Harness:** still omp (same spawn/meter/kill JSONL pattern)
- **Accounting:** direct Anthropic API key (read usage from the API
  rather than omp's agent.db) — or OpenRouter / other provider
- **Policy:** dollar-budget (daily/monthly cap) instead of scavenging
  ramps

The protocol's genericity has been tested by design but not by a second
implementation. Building one would validate or break the interface at:
- `decide()` — does Outlook generalize beyond ramp-based pacing?
- `run()` — does JobClass (HUNT/FIX) make sense for a dollar backend?
- `keep_fresh()` — does it still mean anything without probe staleness?
- `SpendLedger` — are 7 methods too many? Too few?

Worth doing as a spike before advertising the protocol as stable.


## Scheduler: job-level cost tracking and reporting

**Status:** idea

`anticipated_tokens` already tracks historical cost distributions per
kind. Surfacing this in the UI (cost-per-finding, cost-per-repo,
burn-rate charts) would help users understand where budget goes. The
data is all in the jobs table (`tokens_new`, `usage_delta`); it just
needs a `/api/costs` endpoint and a Stats sub-panel.

Related: the `estimate_capacity` calibration (p75 of token/fraction
ratios) is currently only used for the "~Nk tok avail" display. It
could feed into smarter `anticipated_tokens` estimates — the hardcoded
`200k ≈ 10%` conversion in the facade could be replaced by the empirical
calibration, at least for the display path.


## Scheduler: per-repo scan intervals

**Status:** done (scan_interval_days), could be extended

Currently `scan_interval_days` applies uniformly to all repos. Some
repos are more active or more important — per-repo overrides (stored in
the repos table) would let users say "scan this repo every 6h, that one
weekly." The config default stays as the floor.

Similarly, per-kind intervals could be useful: "hunt daily, but only run
dep_update weekly." The current `modernization_interval_days` is already
a per-kind override; generalizing it to a dict (`scan_intervals:
{hunt: 1, dep_update: 7, ...}`) would be clean.


## UI: finding detail page

**Status:** idea

Currently findings are cards in lists. A dedicated detail view (click to
expand full-page) would show: complete evidence/plan text, full job
history with token costs, PR timeline, all recheck results, related
findings in the same file/class. The data is already served by
`/api/finding/<id>` (or could be); the UI just needs the page.


## Process: multi-repo parallelism

**Status:** idea, significant

The scheduler runs one job per cycle. With multiple repos and sufficient
budget, running one job per repo concurrently would increase throughput
without changing the per-repo sequential invariant (which matters for
git worktree safety). Needs:
- Per-repo locking (instead of the current global `_cycle_lock`)
- Budget gate that accounts for all in-flight jobs (already does via
  `running_estimate`)
- Worker process management (multiple concurrent `omp -p` subprocesses)

The backend protocol already supports this (nothing in `decide()` or
`run()` assumes single-threaded dispatch), but the scheduler loop and
the Store's job-state management assume one-at-a-time.
