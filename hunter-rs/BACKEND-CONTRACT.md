# BACKEND-CONTRACT.md — Backend protocol + omp_scavenge for the Rust port

Extracted 2026-09-13 from the Python source at the paths cited below. Companion to `hunter-rs/API-CONTRACT.md` (the read path: error envelope, Row types, `/api/summary` shape — see its §3 (`/api/summary`) and §13.3 (the UI consumer) for where `backend_status_html` crosses the HTTP boundary). Precision bar: a Rust developer should never need to open the Python source. Every claim carries `file:line` (paths relative to `hunter/`; `hunter/…` = `hunter/hunter/…`).

Sources of truth: `hunter/backend.py`; `hunter/backends/omp_scavenge/{__init__,facade,capacity,harness}.py`; `hunter/store.py`; `hunter/types.py`; call sites in `hunter/server.py` + `hunter/scheduler.py`; tests `tests/test_budget.py`, `tests/test_unaccounted_tokens.py`, `tests/test_refresh_stale_probe.py`, `tests/test_store.py`, `tests/test_server.py`.

---

## 0. Where the backend touches the HTTP layer

| Touch point | Site | Needs |
|---|---|---|
| Construction | `serve()` server.py:919-931: `backend = cfg.make_backend(ThreadLocalLedger(cfg))` → `make_server(cfg, backend)`. Same in `daemon()` server.py:1245. Factory types.py:232-243. | `Config::make_backend` + `SpendLedger` impl |
| GET `/api/summary` → `backend_status_html` | `self.backend.status()` server.py:261, emitted :313; typed field server.py:1044; string-checked by `isSummary` (validate.ts:87 — a non-string blanks the dashboard); injected with `{@html}` by StatusPage.svelte:60 on each store refresh | `status()` |
| GET `/api/summary` → next-candidate budget preview | server.py:272-308: when `current_job is None` and `scheduler.pick_next` yields, handler calls `self.backend.decide(anticipated_tokens=scheduler.anticipated_tokens(store, cfg, repo_id, kind))` (:288-290), indexes `outlook.prioritized if override else outlook.normal` (:291), maps to `budget_state: "denied"\|"allowed"`, `budget_reason`, `budget_retry_at`, `is_prioritized: bool(override)` (:292-308). `override = target.get("budget_override")` only for kinds `engage\|harvest\|recheck\|fix` (:285-286). | **`decide()` on the READ path** — live: server.rs:241-246 calls `pick_next` (:241), computes `anticipated_tokens` (:243-245), then awaits `backend.decide(...)` |
| POST `/api/cycle` | server.py:489-518: spawns `scheduler.run_cycle(store, cfg, backend=backend)` in a thread under `_cycle_lock` | full scheduler + `run()` — both landed (§6): `scheduler::run_cycle` scheduler.rs:3316, `Backend::run` backend.rs:158-165 |
| Usage prober thread | `_usage_prober_loop(backend, stop)` server.py:1205-1225; tick `USAGE_PROBE_TICK_S = 60.0` server.py:115; started **only** by `daemon()` (server.py:1258-1260). `serve()` does NOT start it (server.py:919-931). CLI: `serve` = UI-only, `daemon` = UI+scheduler+prober (cli.py:3-6,22-35,54-63). | `keep_fresh()` — landed; the Rust prober is a tokio task inside `run_daemon` (daemon.rs:288-305, `USAGE_PROBE_TICK_S = 60` daemon.rs:18), and `serve` still never probes |
| Denial → daemon sleep | `retry_at` flows through every scheduler `{"denied":…, "retry_at":…}` return into `_compute_sleep_s` server.py:1141-1202: truthy → `sleep = max(60.0, min(retry_at/1000 − time.time() + 30, 3600))` (:1188-1191); `None` → 30 min (:1192-1193) | `Denied.retry_at` semantics |

Scheduler call sites (all ported to `src/scheduler.rs`; they define `decide()`'s contract): run_hunt scheduler.py:283-292 (`outlook.normal`; hunts have no override), run_recheck :415-427, run_fix :904-915, run_engage :1458-1471, run_harvest :1744-1757 (all four: `verdict = outlook.prioritized if override else outlook.normal`; `override = finding.get("budget_override")`, values `'once'|'exempt'|None`, store.py:972-981; `'once'` is cleared after any attempt), `_run_analysis_job` scheduler.py:600-609 (normal). Uniform pattern:
- `Denied(reason, retry_at)` → `log_event('deny', …)`; return `{"denied": reason, "retry_at": retry_at}` — no job row is written (e.g. scheduler.py:287-289). These writes are the **caller's**, not the backend's.
- `Granted(cap_tokens=backend_cap)` → `cap = min(cfg_cap, backend_cap) if backend_cap is not None else cfg_cap` (scheduler.py:292,427,609,915,1471,1757); `cfg_cap` = `hunt_cap_tokens` (hunt/recheck/analysis) or `fix_cap_tokens` (fix/engage/harvest). **Backend never sees config caps; core min()s** (backend.py:47-48).

---

## 1. `backend.py` — the protocol

Module doc (backend.py:1-11): a backend answers (1) may background work spend now, up to how much (`decide`); (2) run this job (`run`); (3) what's your status (`status`/`keep_fresh`). Decisions cross as *data* (Outlook); diagnostics cross as *presentation* (HTML the backend fully owns).

### 1.1 `JobClass` (backend.py:29-37)
`StrEnum`: `HUNT = "hunt"`, `FIX = "fix"`. Scheduler collapses all job kinds (hunt, fix, engage, harvest, recheck, test_gap, …) into these two budget/model classes at the boundary. Lowercase strings; `cfg.model_for(job_class.value)` keys off them (facade.py:311).

### 1.2 `Granted` (backend.py:41-54, frozen dataclass)
| field | type | default | semantics |
|---|---|---|---|
| `cap_tokens` | `int \| None` | `None` | Backend's own spend ceiling for this verdict. `None` = no ceiling (e.g. unlimited local model). Core still min()s against its config caps. |
| `reason` | `str` | `"ok"` | Prose → job notes + UI. **Never machine-matched.** |

### 1.3 `Denied` (backend.py:58-67, frozen dataclass)
| field | type | default | semantics |
|---|---|---|---|
| `reason` | `str` | required | Prose for notes/events/UI. Never machine-matched (tests substring-match only). |
| `retry_at` | `float \| None` | `None` | **Epoch ms** — best-known time the denial resolves. Drives `_compute_sleep_s` (server.py:1180-1193). `None` = no informed estimate → generic 30-min backoff. |

### 1.4 `Verdict = Granted | Denied` (backend.py:70)
Rust: `enum Verdict { Granted { cap_tokens: Option<i64>, reason: String }, Denied { reason: String, retry_at: Option<f64> } }` (`retry_at` is float math in Python and passes through JSON as-is; i64 ms is acceptable if formatting stays equivalent).

### 1.5 `Outlook` (backend.py:74-86)
`{ normal: Verdict, prioritized: Verdict }`. **INVARIANT: prioritized is at least as permissive as normal — if normal is Granted, prioritized must also be Granted** (backend.py:77-78). Scheduler indexes `outlook.prioritized if override else outlook.normal` and never tells the backend which it wanted (backend.py:80-82; scheduler.py:420). omp_scavenge guarantees it by construction (§2.2 decide).

### 1.6 `SpendLedger` (backend.py:95-148) — narrow port from Store
Purpose (backend.py:96-103): compute spend the provider's probe hasn't seen; log window observations + calibration samples; estimate window capacity. Implemented by `Store`; `ThreadLocalLedger` (store.py:1604-1665) wraps it with one Store/SQLite connection per thread (scheduler loop, prober, HTTP handlers — store.py:1607-1610; per-thread `threading.local` cache :1616-1625, plain delegation :1629-1665). **Rust: an sqlx pool makes ThreadLocalLedger unnecessary — implement SpendLedger directly on the pool-backed Store.**

Exact Store SQL (`now_ms() = int(time.time()*1000)`, types.py:90-91):

**`running_estimate() -> int`** (proto backend.py:105-107; impl store.py:1436-1441)
```sql
SELECT COALESCE(SUM(cap_tokens), 0) AS total FROM jobs WHERE state = 'running'
```

**`finished_since(ts_ms) -> int`** (backend.py:109-111; store.py:1443-1459). Strictly-after. `denied` rows have NULL `tokens_new` → SUM skips them; `queued` rows have NULL `finished_at` → excluded by the comparison.
```sql
SELECT COALESCE(SUM(tokens_new), 0) AS total FROM jobs
 WHERE state != 'running' AND tokens_new IS NOT NULL AND finished_at > ?
```

**`finished_between(start_ms, end_ms) -> int`** (backend.py:113-115; store.py:1461-1474) — half-open `(start, end]`:
```sql
SELECT COALESCE(SUM(tokens_new), 0) AS total FROM jobs
 WHERE state != 'running' AND tokens_new IS NOT NULL AND finished_at > ? AND finished_at <= ?
```

**`log_window_observation(limit_id, used_fraction: f64?, status: str?, resets_at: i64?, age_s: f64)`** (backend.py:117-126; store.py:1476-1490), then `commit()`:
```sql
INSERT INTO window_log (observed_at, limit_id, used_fraction, status, resets_at, source_age_s)
VALUES (?,?,?,?,?,?)  -- observed_at = now_ms(); source_age_s = int(age_s)  (TRUNCATION, not round)
```

**`last_window_observation(limit_id, resets_at) -> (observed_at: i64, used_fraction: f64) | None`** (backend.py:128-131; store.py:1492-1503). `None` if no row **or newest row's used_fraction is NULL** (:1501-1502). ±5000 ms groups same-cycle observations:
```sql
SELECT observed_at, used_fraction FROM window_log
 WHERE limit_id = ? AND resets_at BETWEEN ? AND ?  -- resets_at−5000 .. resets_at+5000
 ORDER BY observed_at DESC LIMIT 1
```

**`record_calibration_sample(limit_id, window_resets_at, used_fraction_delta, hunter_tokens)`** (backend.py:133-142; store.py:1505-1521), then `commit()`:
```sql
INSERT INTO calibration_samples (observed_at, limit_id, window_resets_at, used_fraction_delta, hunter_tokens)
VALUES (?,?,?,?,?)  -- observed_at = now_ms()
```

**`estimate_capacity(limit_id, min_delta=0.02, sample_limit=200) -> f64 | None`** (backend.py:144-148; store.py:1397-1434; Rust `Store::estimate_capacity`, store.rs:2393-2445, where `sample_limit` is the constant `SAMPLE_LIMIT = 200` and `min_delta` is not in the signature at all). The implementation is **max tokens hunter ever spent in one completed window cycle** (store.py:1422-1434; the docstrings backend.py:147 and store.py:1400-1405 now say so) — no fraction correlation, no p75, and **`min_delta` is dead** (never referenced in store.py:1397-1434; ThreadLocalLedger just forwards it, store.py:1662-1665). Keep the param for signature parity or delete in both. Algorithm:
1. `period_ms = {"anthropic:5h": 18_000_000, "anthropic:7d": 604_800_000}.get(limit_id)` (`_PERIOD_MS` store.py:1392-1395; store.rs:2396-2401); unknown → `None` (store.py:1407-1409). Called with per-model-class lids (from status()) it correctly returns None.
2. **One statement** (store.rs:2413-2443), not one per cycle: a `cycles` CTE picks the completed cycles from `window_log`, deduping `resets_at` into 10 s buckets and keeping the newest `sample_limit`; a correlated scalar subquery sums hunter's spend inside each cycle's half-open `(resets − period_ms, resets]` window; the outer `MAX` takes the winner. Python still runs the cycle query and then one SUM per cycle (store.py:1413-1433) — same answer, N+1 round trips.
```sql
WITH cycles AS (
    SELECT MIN(resets_at) AS resets
    FROM window_log
    WHERE limit_id = ?1 AND resets_at < ?2         -- now_ms()
    GROUP BY CAST(resets_at / 10000 AS INT)        -- 10 s dedupe bucket
    ORDER BY CAST(resets_at / 10000 AS INT) DESC
    LIMIT ?3                                       -- SAMPLE_LIMIT = 200
)
SELECT COALESCE(MAX(spent), 0) FROM (
    SELECT (SELECT COALESCE(SUM(j.tokens_new), 0) FROM jobs j
             WHERE j.state NOT IN ('denied', 'running')
               AND j.tokens_new IS NOT NULL
               AND j.finished_at > c.resets - ?4   -- period_ms
               AND j.finished_at <= c.resets) AS spent
    FROM cycles c WHERE c.resets IS NOT NULL
)
```
3. `Some(max)` when max > 0, else `None` (store.rs:2444; store.py:1434) — a zero max and no history are the same answer, "no estimate".
4. **`AND j.tokens_new IS NOT NULL` is load-bearing; a port must keep it** (store.rs:2429). `SUM` already skips NULLs, so the clause cannot change the result — which is exactly what makes it look like dead weight. It is what makes the query's `WHERE` imply the predicate of the partial index `jobs_finished_at ON jobs(finished_at) WHERE finished_at IS NOT NULL AND tokens_new IS NOT NULL` (migrations/006_jobs_finished_at_index.sql:10-11), and SQLite will only use a partial index when the query implies its predicate. Delete it and the per-cycle subquery silently degrades from `SEARCH j USING INDEX jobs_finished_at` to `SCAN j` — every job the daemon has ever run, once per cycle, behind the endpoint the UI polls every 5 s, with no test result changing. Measured on a copy of the live database (4320 jobs, 200 cycles for `anthropic:5h`): identical result 11010942 either way, 6.7 ms with the clause, 130.3 ms without. The same clause carries `finished_since`/`finished_between` for the same reason (store.rs:2285-2316), and those two plans are pinned against `SCAN` by tests/store_test.rs:258-319.

**Why one statement.** The per-cycle shape costs one inner query per cycle. On the live database that is 200 per `anthropic:5h` call — already at the `sample_limit` ceiling, so it stops growing but never shrinks — and 18 per `anthropic:7d` call, 218 for one pass over both lids; `status_html` and `decide` between them call `estimate_capacity` up to six times per `GET /api/summary`, the endpoint the UI polls every 5 s (store.rs:2403-2407). Consolidating that N+1 into the statement above took the endpoint from 626 ms to 70 ms. A port that reintroduces the per-cycle loop is correct and slow; dropping the index clause in step 4 is the difference between slow and unusable.

Backing tables (schema.sql): `window_log` :123-131 (`id, observed_at INTEGER NOT NULL, limit_id TEXT NOT NULL, used_fraction REAL, status TEXT, resets_at INTEGER, source_age_s INTEGER` — "mirror of budget observations at decision time"), `calibration_samples` :143-150 (`id, observed_at, limit_id, window_resets_at, used_fraction_delta REAL NOT NULL, hunter_tokens INTEGER NOT NULL`; header comment :133-142: informational only, never gates decisions), `jobs` :98-117 (`state ∈ queued|running|done|failed|killed|denied`, `cap_tokens`, `tokens_new`, `finished_at`, `usage_delta REAL`).

### 1.7 `Backend` protocol (backend.py:157-212)
```python
def decide(self, *, anticipated_tokens: int) -> Outlook      # :160-170
def run(self, cwd: Path, prompt: str, *, cap_tokens: int,
        max_wall_s: int, job_class: JobClass) -> RunResult   # :172-187  (Rust: backend.rs:158-165)
def keep_fresh(self) -> bool                                 # :189-198
def status(self) -> str                                      # :200-212
```
- `decide`: `anticipated_tokens` = caller's pre-reservation for the job under decision (realistic historical estimate, cache-warmth aware). Returns both verdicts; each Granted carries the backend's own cap (backend.py:161-169).
- `run`: `cap_tokens` is the **effective** cap (already min'd by caller); `job_class` drives model selection — backend owns model config (:183-185).
- `keep_fresh`: refresh stale accounting, log observations, record calibration. Called from the server's dedicated prober thread on a fixed tick, decoupled from dispatch cadence. Returns True iff a refresh was performed (:190-197).
- `status`: HTML fragment; UI injects via innerHTML each 5 s poll. Disciplines (:206-210): every interpolated datum HTML-escaped (backend's job); backend-specific class prefix; stateless-render-safe (rebuilt every tick).

### 1.8 `RunResult` (types.py:247-257)
`exit_code: int|None`; `killed_reason: str|None` = `None|"cap"|"wallclock"`; `tokens_new: int` (input+output+cacheWrite from worker ledger); `calls: int`; `session_file: str|None`; `duration_s: float`; `stdout_tail: str = ""`; `usage_delta: float|None = None` (provider 7d used_fraction change during the job; set by facade's snapshot sandwich, facade.py:310-321). Consumed by scheduler `_record_job` (scheduler.py:92-115; state = "killed" if killed_reason else "done"/"failed" by exit_code==0, :86-89).

**Rust addition:** `killed_reason` also takes `"unmetered"`, when the worker's session ledger never appears within the discovery grace period. Python carried on with `tokens_new = 0`, which silently disarms the token cap and books the job as free; the port kills the worker and records the run as killed instead.

### 1.9 What feeds `anticipated_tokens` (scheduler.py:45-83 — stays in core)
Warm iff this exact `(repo_id, kind)` finished a non-denied job within `cfg.cache_ttl_s`: `SELECT 1 FROM jobs WHERE repo_id = ? AND kind = ? AND finished_at > ? AND state != 'denied' LIMIT 1` (:63-71, cutoff `now_ms() − cache_ttl_s*1000`). History = all `tokens_new` for the kind, ascending (:72-79). Empty → 0; else `history[min(int(len * (0.5 if warm else 0.9)), len−1)]` — warm p50, cold p90 (:80-83).

---

## 2. `backends/omp_scavenge/`

Package inventory:
- `__init__.py` (1-16): docstring (harness spawn/meter/SIGTERM; accounting reads agent.db, rolls forward expired cycles, computes unaccounted; policy = dual linear ramps, prioritized waives pacing not exhaustion); `from .facade import OmpScavengeBackend`; `__all__ = ["OmpScavengeBackend"]`.
- `capacity.py` (214 lines) — window reading + pure ramp math. Ported: `capacity.rs`.
- `facade.py` (529 lines) — `OmpScavengeBackend` (decide/run/keep_fresh/status). Ported whole, `run` and `_usage_snapshot` included: `facade.rs` (`impl Backend` :655, `run` :735, `_usage_snapshot_sync` :384).
- `harness.py` (243 lines) — worker subprocess harness. Ported: `harness.rs` (`run_worker` :241).

### 2.1 `capacity.py`

Policy doc (capacity.py:1-31): 7d ramp `allowed = elapsed_fraction_of_window`; 5h ramp `allowed = max(0, (elapsed − HEADROOM) / (5h − HEADROOM))` — zero during human headroom, then 0→1. Both reduce to `allowed > effective_used`. "exhausted" folds into effective_used as exactly 1.0 (no special case; since ramps cap at 1.0 reached only at resets_at, an exhausted window denies for its whole remaining duration). A window's last reading is never discarded for age — usage only increases within a window, so it stays a valid floor; combined with unaccounted-token tracking and wall-clock ramps, denials self-resolve. No active 5h window (nothing ever probed) → allow (opens one), gated only by 7d. Expired account-wide cycles are ROLLED FORWARD, not dropped (per-model-class dims stay dropped). Missing data entirely → deny.

Constants:
- `OMP_AGENT_DB = Path.home() / ".omp/agent/agent.db"` (capacity.py:44) — make injectable in Rust (tests monkeypatch it).
- `HEADROOM_MS = 30*60*1000 = 1_800_000` (:66) — THE tunable (comment :61-65: everything else auto-computes).
- `WEEK_MS = 604_800_000` (:68); `FIVE_HOUR_MS = 18_000_000` (:69); `_RAMP_MS = FIVE_HOUR_MS − HEADROOM_MS = 16_200_000` (4.5 h) (:70).

`WindowState` (capacity.py:48-58): `limit_id: str`; `used_fraction: float|None`; `status: str|None` ("ok"|"exhausted"|…); `resets_at: int|None` (epoch ms); `recorded_at: int` (epoch ms, when omp probed); `age_s: float = 0.0`. Property `stale = age_s > 1800` (:57-58) — **dead code, referenced nowhere** (grep-verified); real staleness uses `cfg.stale_after_s`.

`read_windows() -> dict[str, WindowState]` (capacity.py:73-148):
1. Missing DB file → `{}` (:75-76).
2. sqlite read-only URI `file:{path}?mode=ro` (:78), row factory dict; ANY `sqlite3.Error` → `{}` (:87-88).
3. Query (:80-85):
```sql
SELECT limit_id, used_fraction, status, resets_at, MAX(recorded_at) AS recorded_at
FROM usage_history WHERE limit_id LIKE 'anthropic:%' GROUP BY limit_id
```
⚠️ Relies on SQLite bare-column semantics: with a lone MAX() aggregate, non-aggregated columns come from the max row. Rust must select "the row with greatest recorded_at per limit_id" explicitly (correlated subquery or window function).
4. `usage_history` columns hunter depends on (full DDL replicated in fixture tests/test_budget.py:529-539): `id INTEGER PK AUTOINCREMENT, recorded_at INTEGER NOT NULL, provider TEXT NOT NULL, account_key TEXT NOT NULL, limit_id TEXT NOT NULL, label TEXT NOT NULL, used_fraction REAL NULL, status TEXT NULL, resets_at INTEGER NULL`. Hunter reads only `limit_id, used_fraction, status, resets_at, recorded_at`. Observed lids: `anthropic:5h`, `anthropic:7d`, `anthropic:7d:<model-class>`.
5. `now = time.time()*1000` (:89). Per row, expired-cycle handling if `resets_at` truthy AND `resets_at <= now` (:96-139):
   - `period = {"anthropic:5h": FIVE_HOUR_MS, "anthropic:7d": WEEK_MS}.get(limit_id)`; per-model-class/unrecognized → **drop the row** (`continue`, :130-132) — genuinely-abandoned dims must not gate or show as synthesized-fresh in the UI.
   - Roll forward (:133-139): `new = resets_at; while new <= now: recorded_at = new; new += period`; then `resets_at = new; used_fraction = 0.0; status = "ok"`. `recorded_at` = the CURRENT cycle's actual start boundary (NOT `now`) so `finished_since(recorded_at)` sums every job since rollover (:118-123). Incident rationale (:97-129): dropping the row made decide() fall through to "no active 5h → allow", bypassing unaccounted tracking → ~2.9 M tokens spent with zero denials.
6. `WindowState(…, age_s=(now − recorded_at)/1000)` (:140-147). Rows with NULL resets_at are kept verbatim (no rollover branch).

Pure math (already parameterized on now_ms — keep pure in Rust):
- `ramp_7d(resets_at, now_ms) -> f64` (:151-162): `not resets_at or resets_at <= now_ms → 1.0` (permissive fallback; expired rows never reach here for gating thanks to read_windows); else `min((now_ms − (resets_at − WEEK_MS)) / WEEK_MS, 1.0)`.
- `ramp_5h(resets_at, now_ms) -> f64 | None` (:165-173): `not resets_at or resets_at <= now_ms → None` ("no active window" → decide treats as opener/always-allow); else `elapsed = FIVE_HOUR_MS − (resets_at − now_ms)`; `max(0.0, (elapsed − HEADROOM_MS) / _RAMP_MS)`.
- `retry_at_7d(resets_at, effective_used) -> f64 | None` (:176-185): `None` if falsy resets_at (callers fall back to generic backoff); else `min((resets_at − WEEK_MS) + effective_used * WEEK_MS, resets_at)` — exact inverse of ramp_7d, clamped so it never lands past the reset (test-verified round trip).
- `retry_at_5h(resets_at, effective_used) -> f64 | None` (:188-194): `None` if falsy; else `min((resets_at − FIVE_HOUR_MS) + HEADROOM_MS + effective_used * _RAMP_MS, resets_at)` — exact inverse of ramp_5h, clamped likewise.
- `effective_used(w, inflight_reservation) -> f64` (:197-214): `used = 1.0 if w.status == "exhausted" else (w.used_fraction if not None else 0.0); used + inflight_reservation`. Exhausted clamps to exactly 1.0 — Anthropic's hard-stop signal, raw value unreliable (observed 1.57 on a non-Anthropic limit); trusting it verbatim risks ramp catch-up second-guessing a hard stop or retry_at past the real reset (:200-206). ⚠️ `status≠exhausted ∧ used_fraction=None` floors to 0.0 (:212-213) rather than raising, and is unreachable anyway: all call sites guard (facade.py:172-173, :203, :252-253). Rust: keep the guard at call sites and floor `None` to 0.0 (`unwrap_or(0.0)`).

### 2.2 `facade.py` — `OmpScavengeBackend`

Constants: `_TOK_PER_FRAC_5H = 200_000 / 0.10 = 2_000_000.0` (:42; comment :39-41 — 200 k ≈ 10% of a 5h window; 7d is 168h/5h = 33.6× larger); `_5H_7D_RATIO = capacity.FIVE_HOUR_MS / capacity.WEEK_MS = 5/168 ≈ 0.0297619` (:43); `_esc(s) = html.escape(str(s))` (:46-48 — escapes `& < > " '`); `_CALIBRATION_DURATIONS_MS = {"5h": capacity.FIVE_HOUR_MS, "7d": capacity.WEEK_MS}` (:55-58, moved from store — Anthropic-window knowledge). `@dataclass OmpScavengeBackend { cfg: Config, ledger: SpendLedger }` (:86-94). Logger `hunter.backend` (:37).

**`_unaccounted_fraction(windows, anticipated) -> (res_5h, res_7d)`** (:68-98). Inflight reservation as a fraction, PER WINDOW, each against its own probe time:
1. `running = ledger.running_estimate()` (:111).
2. `fallback = min(w.recorded_at for w in windows.values(), default=0)`; `probe_at_5h = windows["anthropic:5h"].recorded_at` if present else fallback; likewise 7d (:113-117). (Regression fix: per-window probe_at, never a shared min — see test 47 below.)
3. `base = running + anticipated` (:119); `unaccounted_5h = base + ledger.finished_since(probe_at_5h)` (:120); `unaccounted_7d = base + ledger.finished_since(probe_at_7d)` (:121).
4. `cap_5h = ledger.estimate_capacity("anthropic:5h") or _TOK_PER_FRAC_5H` (:93); `cap_7d = ledger.estimate_capacity("anthropic:7d") or (cap_5h / _5H_7D_RATIO)` (:94). Note `or`: None and 0 both fall back.
5. `reservation_X = unaccounted_X / cap_X if cap_X else 0.0` (:96-97).

**`_decide_inner(windows, res_5h, res_7d, *, prio) -> Verdict`** (:100-176). One verdict: 7d ramps first, then 5h. `prio=True`: pacing denials become grants (ramp waived to the 1.0 hard limit) but exhaustion denials stand (:105-109).
1. `now_ms = time.time() * 1000` (:165).
2. `if not windows: return Denied("no window data -- deny until fresh")` (:167-168) — ASCII double hyphen, `retry_at=None`.
3. **7d pass** (:170-199) — iterate `windows.items()` in dict order (insertion order = SQL GROUP BY order, ascending limit_id in practice; **Rust: use BTreeMap** — among multiple over-ramp `:7d` lids the first wins the reason string):
   - consider only lids containing `":7d"`; skip if `w.status != "exhausted" and w.used_fraction is None` (:172-173);
   - skip expired cycles `if w.resets_at and w.resets_at <= now_ms` (:174-175; belt-and-braces vs read_windows — covers per-model-class rows read_windows kept because their resets_at was future at read time);
   - `elapsed_frac = ramp_7d(w.resets_at, now_ms)`; `effective_used = _effective_used(w, res_7d)` (:118-119);
   - if `effective_used >= elapsed_frac` (:178): `is_exhausted = (w.status == "exhausted")`; `retry = w.resets_at if is_exhausted else retry_at_7d(w.resets_at, effective_used)` (:179-184); reason (:185-189) EXACTLY:
     `f"{lid}: used {effective_used - reservation_7d:.2f} + unaccounted {reservation_7d:.2f} = {effective_used:.2f} >= ramp {elapsed_frac:.2f}"`
     If `prio and not is_exhausted` (:132-138): `headroom_frac = max(0.0, 1.0 − effective_used)`; `cap = _frac_to_tokens(headroom_frac, "7d")`; `cap <= 0 → Denied(reason, retry_at=retry)`; else `Granted(cap_tokens=cap, reason=f"prio override ({reason})")`. Else `Denied(reason, retry_at=retry)` (:139).
4. **5h pass** (:143-168): `w5 = windows.get("anthropic:5h")`; gate only if `w5 is not None and (w5.status == "exhausted" or w5.used_fraction is not None)` (:144-145); `allowed = ramp_5h(w5.resets_at, now_ms)`; `allowed is None` → opener, skip (:146-147); `effective_used = _effective_used(w5, res_5h)`; deny iff `effective_used >= allowed` (:148-149); `retry = w5.resets_at if exhausted else retry_at_5h(w5.resets_at, effective_used)` (:150-155); reason (:156-160) EXACTLY (label literal `5h`, not the lid):
   `f"5h: used {effective_used - reservation_5h:.2f} + unaccounted {reservation_5h:.2f} = {effective_used:.2f} >= ramp {allowed:.2f}"`
   Same prio-waiver structure with dim "5h" (:161-167).
5. **All passed** (:170-176): `headroom = _compute_headroom(windows, res_5h, res_7d, prio=prio)`; `return Granted(cap_tokens=headroom, reason="ok")` — headroom may be None (unbounded) when no window had a usable used_fraction.

**`_compute_headroom(windows, res_5h, res_7d, *, prio) -> int | None`** (:178-212). Min headroom in tokens across windows. Fresh `now_ms = time.time()*1000` (:186 — second clock read inside one decide()). Per window: skip `used_fraction is None` (:189-190). `:7d` lids: `ceiling = 1.0 if prio else ramp_7d(w.resets_at, now_ms)`; `frac = max(0.0, ceiling − _effective_used(w, res_7d))`; `tok = _frac_to_tokens(frac, "7d")` (:191-196). `:5h` lids: `allowed = ramp_5h(…)`; only if not None: `ceiling = 1.0 if prio else allowed`; same with res_5h, dim "5h" (:197-204). `return min(caps) if caps else None` (:212).

**`_frac_to_tokens(frac, dim) -> int`** (:214-220): `cap_5h = ledger.estimate_capacity("anthropic:5h") or _TOK_PER_FRAC_5H`; `cap_7d = ledger.estimate_capacity("anthropic:7d") or (cap_5h / _5H_7D_RATIO)`; `dim == "5h" or ":5h" in dim` → `int(frac * cap_5h)`; else → `int(frac * cap_7d)`. Both caps are derived exactly as in `_unaccounted_fraction` (:94-99): the 7d estimate is consulted, and the 5h-times-ratio value is only the fallback when there is not yet enough history to estimate one. (An earlier revision of this document claimed the 7d estimate was *not* consulted here and told the port to reproduce that asymmetry verbatim. It was a mis-transcription: facade.py:224 has always read the estimate. The Rust port matches the source, not that sentence.)

**`decide(*, anticipated_tokens) -> Outlook`** (:278-296):
1. `windows = capacity.read_windows()` (:224); `res_5h, res_7d = _unaccounted_fraction(windows, anticipated_tokens)` (:225).
2. `normal = _decide_inner(prio=False)` (:283).
3. Monotonicity by construction (:284-294): if normal is Granted → `prioritized = normal`; then `prio_headroom = _compute_headroom(prio=True)` (:288); replace with `Granted(cap_tokens=prio_headroom, reason="ok")` iff `prio_headroom is not None and (normal.cap_tokens is None or prio_headroom > normal.cap_tokens)` (:289-292). (Edge: normal cap None ⇒ prio headroom is None too — the per-window skip conditions are identical — so prioritized stays == normal; invariant can't break.) If normal is Denied → `prioritized = _decide_inner(prio=True)` (:293-294).
4. `Outlook(normal=normal, prioritized=prioritized)` (:296).

Verdict quick reference (tests substring-match `"5h"`, `"7d"`, `"ramp"`, `"no window data"`, prefix `"anthropic:7d"` — keep formats byte-identical; Python `:.2f` rounds half-to-even, use `f64` round-ties-even formatting in Rust):
| situation | verdict | reason | retry_at / cap |
|---|---|---|---|
| no windows | Denied | `no window data -- deny until fresh` | retry None |
| :7d over ramp (pacing) | Denied | `{lid}: used {u:.2f} + unaccounted {r:.2f} = {eff:.2f} >= ramp {ramp:.2f}` | `retry_at_7d(resets, eff)` |
| :7d exhausted | Denied | same format (eff = 1.0 + r) | `resets_at` |
| :7d pacing + prio, 1−eff yields cap>0 | Granted | `prio override ({7d reason})` | cap = `_frac_to_tokens(1−eff, "7d")` |
| 5h over ramp (pacing) | Denied | `5h: used {u:.2f} + unaccounted {r:.2f} = {eff:.2f} >= ramp {ramp:.2f}` | `retry_at_5h(resets, eff)` |
| 5h exhausted | Denied | same | `resets_at` |
| 5h pacing + prio, cap>0 | Granted | `prio override ({5h reason})` | cap dim "5h" |
| prio waiver but cap ≤ 0 | Denied | the pacing reason | the pacing retry |
| all pass | Granted | `ok` | cap = min-headroom or None |

**`run(cwd, prompt, *, cap_tokens, max_wall_s, job_class) -> RunResult`** (:300-322) — ported at facade.rs:735-779. Usage-delta sandwich: `pre = _usage_snapshot()` (:310); `model = cfg.model_for(job_class.value)` (:311); `rr = run_worker(cfg, cwd, prompt, cap_tokens, max_wall_s, model=model)` (:312-319); `post = _usage_snapshot()` (:320); `rr.usage_delta = post − pre` iff both non-None else None (:321). `_usage_snapshot() -> float|None` (:324-332): fresh `read_windows()`; max `used_fraction` over `:7d` lids; None if none. Only caller is run().

**`keep_fresh() -> bool`** (:336-358):
1. `windows = capacity.read_windows()` (:338).
2. `self._observe(windows)` (:341) — ALWAYS (observations + calibration logged even when fresh).
3. Staleness gate on `anthropic:5h` only: `w5 = windows.get("anthropic:5h"); if w5 is not None and w5.age_s <= cfg.stale_after_s: return False` (:344-346). Missing 5h (or no windows) → probe. Boundary: `age_s == stale_after_s` counts as fresh (`<=`).
4. `from hunter.util import run_cmd` (deferred import :348 — tests patch `hunter.util.run_cmd`).
5. `run_cmd([cfg.omp_bin, "usage", "invalidate", "--provider", "anthropic"], timeout=15)` (:350-353) — **rc ignored** (best-effort cache bust).
6. `rc, _out = run_cmd([cfg.omp_bin, "usage", "--provider", "anthropic"], timeout=30)` (:354-357).
7. `return rc == 0` (:358).
Why two commands (test_refresh_stale_probe.py:1-15): headless `omp -p` never refreshes usage_history (confirmed in production); plain `omp usage` can serve omp's own internal cache — invalidate must precede the read; gated on staleness because Anthropic rate-limits /usage per source IP. `run_cmd` (util.py:9-40): subprocess.run, stdout+stderr merged text, `check=False`; TimeoutExpired → `(124, msg)`; OSError → `(127, str(e))`; **never raises**. Only rc reaches keep_fresh.

**`_observe(windows)`** (:360-398) — moved from store.log_window (:363-365):
- `now = int(time.time() * 1000)` (:367).
- Per window (dict order): `horizon = next((h for h in _CALIBRATION_DURATIONS_MS if f":{h}" in w.limit_id), None)` (:368-374) — `"5h"` matches `anthropic:5h`; `"7d"` matches `anthropic:7d` AND `anthropic:7d:<model>` (iteration order "5h" then "7d"; no lid contains both).
- Calibration iff `horizon and w.resets_at and w.used_fraction is not None` (:375): `prev = ledger.last_window_observation(w.limit_id, w.resets_at)` (:376); iff `prev is not None and w.used_fraction > prev[1] and (now − prev[0]) <= _CALIBRATION_DURATIONS_MS[horizon]` (:377-381): `tok = ledger.finished_between(prev[0], now)` (:382); iff `tok > 0`: `ledger.record_calibration_sample(w.limit_id, w.resets_at, w.used_fraction − prev[1], tok)` (:383-389).
- ALWAYS: `ledger.log_window_observation(w.limit_id, w.used_fraction, w.status, w.resets_at, w.age_s)` (:392-398).

### 2.3 `status() -> str` (:402-520) — exact HTML contract

No windows → return exactly `<div class="scv-note">No window data available</div>` (:405-406).

Otherwise: `now_ms = time.time()*1000` (:408); `res_5h, res_7d = _unaccounted_fraction(windows, 0)` (:413) — anticipated=0 on purpose: bars show observable state (probe + actual in-flight), not the gate's hypothetical reservation (:409-412). Iterate `sorted(windows.items(), key=lambda kv: kv[0])` — **ascending limit_id** (:416). Per window:
- `label = lid.replace("anthropic:", "") + " window"` (:417) → "5h window", "7d window", "7d:model-class window".
- `used_pct = f"{w.used_fraction*100:.0f}%"` else `"?"` when used_fraction is None (:418).
- Dimension select (:421-437): `":5h" in lid` → `unacct=res_5h`, `ramp=ramp_5h(w.resets_at, now_ms)`, `elapsed_frac=(FIVE_HOUR_MS − (resets_at − now_ms))/FIVE_HOUR_MS` if `resets_at and resets_at > now_ms` else None; elif `":7d" in lid` → `unacct=res_7d`, `ramp=ramp_7d(…)`, `elapsed_frac=None` (7d has no headroom); else `unacct=0.0, ramp=None, elapsed_frac=None`.
- `fill_pct = min(100, round((w.used_fraction or 0) * 100))` (:439). ⚠️ Python `round()` = round-half-to-even; Rust `f64::round()` is half-away-from-zero — use `round_ties_even` for byte parity.
- `soft_pct = min(100 − fill_pct, max(0, round(unacct * 100)))` (:440).
- `ramp_pct = min(100, round(ramp * 100))` if ramp is not None else None (:441).
- `avail_frac = max(0.0, (ramp if ramp is not None else 1.0) − (w.used_fraction or 0) − unacct)` (:444-446); `avail_pct = f"{avail_frac*100:.0f}%"` (:447).
- `cap = ledger.estimate_capacity(lid)` (:448) — FULL lid, so per-model-class lids → None → no token annotation. `avail_tok = avail_frac * cap` if cap not None (:449).
- `avail_str` (:450-458): `""` if used_fraction is None; else `f" \u00b7 {avail_pct} avail (~{_fmt_tokens(avail_tok)} tok)"` when avail_tok not None, else `f" \u00b7 {avail_pct} avail"` (\u00b7 = MIDDLE DOT).
- Tone (:461-465): `is_stale = w.age_s > cfg.stale_after_s`; `is_exhausted = w.status == "exhausted" or (w.used_fraction is not None and w.used_fraction >= 1.0)`; `tone = "stale" if is_stale else ("bad" if is_exhausted else "ok")` — stale wins over bad.
- `probe_age = f"{w.age_s/60:.0f}m ago"` (:468).
- `reset_str` (:471-483): resets_at set → `remain_s = (resets_at − now_ms)/1000`; `reset_abs = time.strftime("%I:%M %p", time.localtime(resets_at/1000)).lstrip("0")` (:473-475 — **LOCAL timezone, 12-hour clock**, e.g. "3:05 PM"); remain>0 → `h = remain_s//3600, m = (remain_s%3600)//60` (ints), `countdown = f"{h}h{m:02d}m" if h else f"{m}m"`, `reset_str = f"resets {reset_abs} ({countdown})"`; remain≤0 → `"resetting"`. No resets_at → `"reset unknown"`.
- `headroom_str` (:486-491): only when `elapsed_frac is not None and ramp is not None and ramp == 0.0 and elapsed_frac > 0`: `headroom_remain_ms = HEADROOM_MS − elapsed_frac*FIVE_HOUR_MS`; if > 0: `f" \u00b7 headroom {int(headroom_remain_ms/60_000)}m"`.
- `unacct_str = f" +{unacct*100:.0f}% in flight"` iff `unacct > 0.005` else `""` (:494).
- `marker = f'<i class="scv-ramp" style="left:{ramp_pct}%"></i>'` iff `ramp_pct is not None and ramp_pct > 0` else `""` (:498-502; hidden at 0 — during headroom it would sit invisibly behind the fill).
- Per-window fragment, exact concatenation (:503-518):
```html
<div class="scv-win"><div class="scv-lab"><b>{esc(label)}</b><span>{esc(used_pct)} used{esc(unacct_str)}{esc(avail_str)}{" \u26a0\ufe0fstale" if is_stale else ""}</span></div><div class="scv-bar"><i class="scv-fill scv-{tone}" style="width:{fill_pct}%"></i><i class="scv-soft" style="width:{soft_pct}%"></i>{marker}</div><div class="scv-sub">{esc(reset_str)}{esc(headroom_str)} \u00b7 probed {esc(probe_age)}</div></div>
```
Escaping: `label, used_pct, unacct_str, avail_str, reset_str, headroom_str, probe_age` all pass `_esc` (html.escape incl. quotes); the `\u26a0\ufe0f` + `stale` literal and all markup are raw; the `\u00b7` before "probed" is raw markup. Join fragments with `"\n"` (:520).
`_fmt_tokens(n)` (:523-529): `n ≥ 1_000_000` → `f"{n/1_000_000:.1f}M"`; `n ≥ 1_000` → `f"{n/1_000:.0f}k"`; else `str(int(n))`.

Derivable typed model for the Rust renderer (compute-then-render; render must still emit the exact markup above):
```rust
struct WindowPanel { label: String, used_pct: String /* "37%" | "?" */, unacct_note: String /* "" | " +N% in flight" */, avail_note: String /* "" | " · N% avail…" */, stale: bool, tone: Tone /* Ok|Bad|Stale */, fill_pct: u8, soft_pct: u8, ramp_pct: Option<u8>, reset_str: String, headroom_note: String, probe_age: String }
```
UI dependency map (class names are load-bearing): every rule lives in `StatusPage.svelte`'s style block as a `:global(...)` selector, because the markup arrives as a string from the daemon and Svelte's scoping would otherwise strip rules it cannot see applied — `.scv-win` :233; `.scv-lab`/`b` :234-235; `.scv-bar` :236; `.scv-fill` :237 + tones `.scv-ok` #4e8 / `.scv-bad` #e54 / `.scv-stale` #888 :238-240; `.scv-soft` striped overlay :241; `.scv-ramp` 2px white marker :242; `.scv-sub` :243; `.scv-note` :244. Consumer: the fragment is type-checked as a string by `isSummary` (validate.ts:87), so a non-string blanks the dashboard rather than rendering partially, and injected by `{@html store.summary.backend_status_html}` (StatusPage.svelte:60) on every store refresh.

### 2.4 `harness.py` — inventory (ported: `harness.rs`)
`SESSIONS_RETAINED = 50` (:41); run directories live under `sessions_root(work_root) = <work_root>/sessions`. `ledger_usage(session_file) -> (tokens, calls)` (:45-64): per JSONL line, skip JSON-decode failures (partial trailing line mid-write); count assistant-role records with usage; `tokens += input + output + cacheWrite`; OSError → totals so far. `_run_session_dir(work_root, cwd, spawn_ms)`: `<work_root>/sessions/<cwd-slug>--<spawn_ms>-<seq>`, one directory per run. `prune_sessions(work_root)` runs first and keeps the newest `SESSIONS_RETAINED`, best effort: a transcript that will not delete is a disk-space problem, not a reason to refuse the job. `_ledger_dir_usage(run_dir)` (:122-140): sum every `*.jsonl` in that directory, `None` until one exists; the name-sorted first file is the run's own session. `_kill_tree(proc)` (:143-153): SIGTERM the process group, wait 10 s, SIGKILL fallback. `run_worker(cfg, cwd, prompt, cap_tokens, max_wall_s, model=None)` (:156-243): `cmd = [cfg.omp_bin, "-p", prompt, f"--session-dir={run_dir}"]` + `[f"--model={model}"]` if model + `[f"--smol={cfg.model_smol}"]` if set; TemporaryFile captures stdout+stderr; `start_new_session=True`; poll every `cfg.poll_s` metering the run directory; kill at `tokens >= cap_tokens` ("cap") or wall > max_wall_s ("wallclock"); final re-meter; `stdout_tail = last 2000 chars`.

**Why the private session directory:** omp's `autoResume` setting continues the newest session for the same cwd whenever no session flag or session directory is passed, and a resumed worker re-caches the entire prior transcript on its first call. Observed in production: 508 709 `cacheWrite` tokens on call #1 for a repo whose session had been growing since 2026-09-06, i.e. the cap was already blown before the worker did anything. Passing an empty `--session-dir` per run is what makes each job start cold; it also makes discovery exact, which is why the old `_snapshot`/`_discover` appeared-or-grew heuristic and the `since_iso` ledger filter are gone from both implementations.

**Where they live, and why they are pruned:** under `<work_root>/sessions`, not the operator's `~/.omp/agent/sessions`. A directory per run never reused is unbounded growth, and putting it in a tree a human also uses means nothing prunes it and the operator's own sessions get buried — that tree was already 556 entries / 2.9 GB when this was changed. Worker transcripts are hunter's data, so they live in hunter's work root, and the newest `SESSIONS_RETAINED` are kept: enough to debug a failure noticed days later, which is all these are read for once metering has finished with them.

---

## 3. Config inputs (types.py:173-243)

| config.json path | Config field | default | consumed by |
|---|---|---|---|
| `ompBin` | `omp_bin` | `"omp"` | keep_fresh probe argv facade.py:351,355; worker argv harness.py:168 / harness.rs:273 |
| `budget.staleAfterS` | `stale_after_s` | `300` | keep_fresh gate facade.py:345; status tone/⚠️ facade.py:461 |
| `budget.cacheTtlS` | `cache_ttl_s` | `3600` | scheduler.anticipated_tokens warm/cold split scheduler.py:63-71 (core-side; feeds decide input) |
| `pollS` | `poll_s` | `2.0` | harness meter loop harness.py:226 / harness.rs:390 |
| `models.default` | `model_default` | `None` | `model_for` fallback types.py:195-197 |
| `models.smol` | `model_smol` | `None` | harness `--smol` harness.py:171-172 / harness.rs:279-281 |
| `models.hunt` | `model_hunt` | `None` | `model_for("hunt")` |
| `models.fix` | `model_fix` | `None` | `model_for(kind != "hunt")` |
| `backend.type` | `backend_type` | `"omp-scavenge"` | `make_backend` discriminator types.py:239-243; unknown → `ValueError(f"unknown backend_type: {v!r}")` |
| `hunt.capNewTokens` | `hunt_cap_tokens` | `200_000` | core cap min'd vs Granted.cap (scheduler.py:283,292, …) |
| `fix.capNewTokens` | `fix_cap_tokens` | `150_000` | same for fix/engage/harvest |

`model_for(kind)` (types.py:195-197): `(model_hunt if kind == "hunt" else model_fix) or model_default` — only `"hunt"` picks model_hunt; the only other JobClass value is `"fix"`. Load-mapping cites: types.py:200-230 (ompBin :212, staleAfterS :221, cacheTtlS :222, pollS :224, models.* :225-228, backend.type :229). `make_backend(ledger)` (types.py:232-243): deferred import of OmpScavengeBackend; returns `OmpScavengeBackend(cfg=self, ledger=ledger)`.

⚠️ **The task brief's `budget.deny5hAbove` and weekly-reserve keys DO NOT EXIST** (grep-verified: no `deny5h|reserve|weekly` identifiers under hunter/hunter). The old fixed-threshold+reserve design was superseded by the dual-ramp policy (capacity.py:1-31): the "7d interactive reserve" is structural (the 7d linear ramp never lets hunter get ahead of uniform pacing), and the human-headroom knob is `HEADROOM_MS = 30 min` (capacity.py:66), a code constant, not config.

---

## 4. TEST SPEC — 71 tests; every one ports (constants inline)

**Shared harness, tests/test_budget.py:22-94**: `_NOW_MS` = real `time.time()*1000` at import; `_WEEK_MS/_5H_MS/_1H_MS`; `_cfg(**o)` = Config(work_root=/tmp, db_path=/tmp/test.db, hunt_cap_tokens=200_000, fix_cap_tokens=150_000, **stale_after_s=1800** ← note: test default ≠ prod default 300); `_ws(lid, used_fraction=0.10, status="ok", resets_at=_NOW_MS+_WEEK_MS//2, age_s=60.0)` with `recorded_at=_NOW_MS−age_s*1000`; `_healthy_windows(w5_used=0.05, w5_elapsed_h=4.5)` = {anthropic:5h(used=w5_used, resets=_NOW_MS+(5−elapsed_h)h), anthropic:7d(0.10), anthropic:7d:model-class(0.10)}; `_FakeLedger(running=0, finished=0)`: running_estimate→running, finished_since→finished (**ignores ts**), finished_between→0, estimate_capacity→None (⇒ 2M/67.2M fallback caps: res_5h=finished/2e6, res_7d=finished/(2e6/0.0297619)≈finished/67.2e6), observation methods no-op; `_backend(...)` monkeypatches `capacity.read_windows`.

### tests/test_budget.py (42)
1. `test_empty_windows_deny` :102 — G: windows={} / W: decide(0) / T: normal Denied, reason contains "no window data".
2. `test_stale_5h_low_usage_allows_via_ramp_not_bypass` :109 — 5h used .10 age 3600 s resets now+1h (elapsed 4h ⇒ ramp (4−0.5)/4.5≈0.778), 7d .10 → Granted (staleness never special-cased; live ramp passed the reading).
3. `test_stale_5h_high_usage_still_denies` :124 — same but used .90 ≥ .778 → Denied, "5h" in reason.
4. `test_stale_5h_own_finished_jobs_count_toward_effective_used` :139 — 5h used .10 age 30 s resets now+1h; ledger finished=1_600_000 ⇒ res_5h=.80 ⇒ eff .90 ≥ .778 → Denied "5h".
5. `test_stale_5h_denied_by_7d_ramp` :155 — 5h defaults, 7d used .30 resets now+.95w (ramp .05) → Denied, "7d" in reason (7d pass runs before 5h).
6. `test_5h_and_7d_unaccounted_reservations_are_independent` :168 — (a) healthy 5h(used 0, 4.5h), 7d .02 resets now+.9w (ramp .10), finished=20M ⇒ res_7d≈.298 → Denied "7d"; (b) 5h used 0 elapsed 3.0h (ramp .556), finished=5M ⇒ res_5h=2.5 → Denied "5h".
7. `test_7d_used_above_ramp_deny` :197 — 7d .30 resets now+.9w → Denied; "ramp" in reason; **retry_at ≈ _NOW_MS + 0.20·_WEEK_MS ± 2000** (retry_at_7d inverse).
8. `test_7d_used_below_ramp_allow` :210 — 7d .30 resets now+.5w (ramp .5) → Granted.
9. `test_5h_first_30min_deny` :226 — 5h used .05, elapsed .25h (ramp 0) → Denied, "5h" and "ramp" in reason.
10. `test_5h_at_exactly_30min_deny` :235 — elapsed .5h ramp 0, used .01 ≥ 0 → Denied.
11. `test_5h_harvest_halfway_low_usage_allow` :242 — elapsed 2.75h (ramp .5), used .05 → Granted.
12. `test_5h_harvest_halfway_high_usage_deny` :248 — used .60 ≥ .5 → Denied "5h"+"ramp"; **retry_at ≈ _NOW_MS + 0.45·3600·1000 ± 2000**.
13. `test_5h_harvest_end_high_usage_allow` :257 — elapsed 4.95h (ramp≈.989), used .90 → Granted.
14. `test_5h_exhausted_deny` :264 — 5h used 1.0 status exhausted resets now+w//2 → Denied; **retry_at == resets_at exactly**.
15. `test_5h_exhausted_but_stale_still_denies` :276 — exhausted, age 3600 s, resets now+4 min → Denied; retry_at == resets_at (stale exhausted ≠ opener).
16. `test_7d_denial_during_5h_headroom_uses_7d_retry_not_5h_timing` :294 — 5h used 0 resets now+4.75h (in headroom); 7d .30 resets now+.9w → Denied; reason startswith "anthropic:7d"; retry ≈ now+.20w ± 2 s.
17. `test_no_5h_window_allow` :315 — only 7d(.10) → Granted, cap_tokens not None and > 0.
18. `test_no_5h_window_but_7d_over_deny` :325 — only 7d(.30, resets now+.95w) → Denied "7d".
19. `test_expired_model_class_window_ignored` :343 — healthy + `anthropic:7d:abandoned-model`(used .99, resets now−3w, age 26 d) → Granted (expired :7d row skipped by _decide_inner guard).
20. `test_active_model_class_window_still_gates` :358 — `anthropic:7d:active-model`(.30, resets now+.95w) → Denied "7d".
21. `test_healthy_allow` :375 — `_healthy_windows()` → Granted, cap > 0.
22-26 use a REAL sqlite file: `_make_agent_db` :387-412 (usage_history DDL above; inserts provider='anthropic', account_key='acct', label=limit_id) + `monkeypatch budget_module.OMP_AGENT_DB` :425 etc.
22. `test_read_windows_drops_expired_cycle_window` :414 — rows (anthropic:7d, .3, ok, now+w/2, now−60 s) and (anthropic:7d:fable, .56, ok, now−26 d, now−26 d) → keys == {"anthropic:7d"}.
23. `test_read_windows_keeps_active_window` :434 — single live 7d row → present with used_fraction 0.3.
24. `test_read_windows_rolls_forward_expired_5h_window` :449 — 5h .36 resets now−47 min (recorded resets−1h) → present; used 0.0; status "ok"; resets == old+_5H_MS; **recorded_at == old resets** (new cycle's actual start).
25. `test_read_windows_rolls_forward_expired_7d_window` :485 — 7d .55 resets now−2h → used 0.0; resets old+_WEEK_MS; recorded_at old.
26. `test_read_windows_rolls_forward_through_multiple_missed_cycles` :507 — 5h resets now−2.3·_5H_MS → resets>now, resets−now ≤ _5H_MS (current cycle, not the first missed one), used 0.0.
27. `test_decide_denies_on_unaccounted_alone_through_a_fresh_rollover` :528 — 5h used 0.0 recorded_at=window_start, elapsed 3h (ramp .556); ledger finished=2_900_000 ⇒ res_5h=1.45 → Denied "5h".
28-32. `TestRamp7d` :559 — `ramp_7d(None, now)==1.0` :560; expired(now−1000)→1.0 :563; resets now+w/2 → 0.5±1e-6 :566; resets now+w → 0.0±1e-6 :570; resets now+2w → ≤1.0 (bad-data clamp) :574.
33-37. `TestRamp5h` :581 — None→None :582; expired→None :585; 15 min elapsed→0.0 :588; 2.75h elapsed→0.5±1e-6 :592; just-started (resets now+_5H_MS)→0.0 never negative :597.
38-39. `TestRetryAt7d` :602 — None→None :603; **round-trip** `ramp_7d(resets, retry_at_7d(resets,u)) == u ± 1e-9` for u∈{0,.1,.5,.9}, resets=now+.4w :606.
40-42. `TestRetryAt5h` :617 — None→None :618; round-trip for u∈{0,.25,.5,.9}, resets=now+3.2h :621; `retry_at_5h(resets, 0.0) == window_start + 30·60·1000` (resets=now+4h) :628.

### tests/test_unaccounted_tokens.py (6) — real Store as ledger (fixtures :29-39); `_ws(lid, recorded_at)` used .1, resets recorded+1h :42-49; `_finished_job`/`_running_job` :52-58; `_backing_tokens` inverts fractions via `_TOK_PER_FRAC_5H`/`_5H_7D_RATIO` :61-70 (valid because fresh Store ⇒ estimate_capacity None ⇒ fallback caps)
43. `test_no_jobs_returns_zero` :73 — both probes 1h old, no jobs → (0, 0).
44. `test_running_job_counted_via_cap_tokens_in_both_fields` :83 — running job cap 150_000 → (150000, 150000).
45. `test_anticipated_added_to_both_fields` :100 — anticipated=80_000 → (80000, 80000).
46. `test_finished_job_scoped_to_each_windows_own_probe` :114 — 5h probe 1h ago, 7d probe 2h ago; 50_000 tok finished probe_5h+60 s → (50000, 50000).
47. `test_stale_7d_probe_no_longer_drags_the_5h_baseline_back` :133 — 7d probe 3h ago; 5h rollover 1h ago; 999_999 tok finished rollover−60 s → **(0, 999999)**; then +42_000 finished rollover+60 s → (42000, 1041999). Pins per-window probe_at (no shared min()).
48. `test_falls_back_to_min_when_window_missing` :178 — only 7d present (probe 1h ago); 10_000 tok after → (10000, 10000) (absent 5h falls back to min recorded_at of present windows).

### tests/test_refresh_stale_probe.py (9) — patches `hunter.util.run_cmd` + `capacity.read_windows` (:65-82); `_INVALIDATE = ["omp","usage","invalidate","--provider","anthropic"]`, `_READ = ["omp","usage","--provider","anthropic"]` :32-33; `_ws(age_s)` = 5h used .1 :47-56; ledger = real Store (observe writes flow into tmp DB)
49. `test_no_windows_at_all_forces_a_probe` :85 — {} → True; calls == [INVALIDATE, READ].
50. `test_fresh_window_does_not_force_a_probe` :95 — age 60 vs threshold 1800 → False; no calls.
51. `test_stale_window_forces_a_probe` :106 — age 2000 → True; both calls.
52. `test_exactly_at_threshold_does_not_force` :117 — age 1800 == threshold → False (**strict >** forces).
53. `test_respects_configured_stale_after_s` :131 — age 500: threshold 1800 → False; threshold 300 → True (config knob has real effect).
54. `test_invalidates_before_reading_so_the_read_cannot_serve_a_stale_cache` :148 — calls[0]==INVALIDATE, calls[1]==READ.
55. `test_uses_configured_omp_bin` :164 — omp_bin="/custom/path/omp" → argv[0] replaced in BOTH commands.
56. `test_failed_probe_returns_false` :177 — rc=1 → False (windows stay stale; tolerated via unaccounted tracking).
57. `test_failed_invalidate_does_not_block_the_read_attempt` :189 — invalidate rc 1, read rc 0 → True; both attempted.

### tests/test_store.py — ledger + observe (9)
58. `TestWindowLog.test_log_window_observation` :354 — two observations (5h .3 ok resets 9_999_999 age 5.0; 7d .1 age 10.0) → 2 rows in id order; `source_age_s == 5` (int-truncated).
59. `TestCalibration.test_first_probe_records_no_sample` :392 — single observe(.10) → 0 samples. (Helpers :373-390: `_RESETS_AT = 99_999_999_999`; `_probe(frac)` recorded_at=1, age 1.0; `_observe` builds a REAL OmpScavengeBackend over the Store and calls `backend._observe({"anthropic:5h": probe})`.)
60. `test_fresh_probe_with_hunter_spend_records_a_sample` :397 — observe(.10); job done 500_000 tok; observe(.20) → exactly 1 sample {limit_id anthropic:5h, hunter_tokens 500000, used_fraction_delta ≈ 0.10, window_resets_at _RESETS_AT}.
61. `test_unchanged_used_fraction_records_no_sample` :414 — .10 → spend → .10 again → 0 samples (delta must be strictly > 0).
62. `test_no_hunter_spend_records_no_sample` :426 — .10 → .20 with no jobs → 0 samples (finished_between == 0).
63. `test_different_window_instance_not_compared` :434 — .90 → spend → observe .05 with resets_at+6h → 0 samples (±5 s last_window_observation window misses the new cycle).
64. `test_estimate_capacity_no_data_returns_none` :450.
65. `test_estimate_capacity_returns_max_spend_per_cycle` :453 — window_log rows for two completed 5h cycles (resets now−5h, now−10h); jobs 500_000 and 2_000_000 finished inside each → estimate == 2_000_000.
66. `test_estimate_capacity_scoped_by_limit_id` :478 — completed 7d cycle + 1_000_000 job → estimate("anthropic:5h") None; estimate("anthropic:7d") == 1_000_000.

### tests/test_server.py `TestUsageProberLoop` (5) — the loop lives server-side; the Rust equivalent is the daemon's prober task (daemon.rs:288-305), so these port against `run_daemon`, not `serve`
67. `test_runs_immediately_without_waiting_a_full_tick` :328 — tick monkeypatched to 3600 → exactly 1 keep_fresh call shortly after start.
68. `test_ticks_again_after_the_configured_interval` :352 — tick 0.02 → ≥ 3 calls.
69. `test_stops_promptly_when_the_stop_event_is_set` :374 — stop event → thread exits ≤ 2 s.
70. `test_a_failed_tick_does_not_crash_the_loop` :390 — keep_fresh raising RuntimeError → loop survives, ≥ 2 calls.
71. `test_default_tick_is_within_the_1_to_5_minute_range` :416 — `60.0 <= USAGE_PROBE_TICK_S <= 300.0` (actual 60.0, server.py:111).

Also relevant, NOT backend-behavior tests: scheduler tests each define a local FakeBackend (`keep_fresh→False`, `status→""`, canned decide) — test_followups.py:41, test_recheck_status.py:37, test_run_fix_invariant.py:44, test_run_hunt_rehunt.py:91, test_engage_history_guard.py:34, test_fix_retry_give_up.py:43, test_override_wake.py:32 — the mock pattern the Rust core tests took over as `ScriptedBackend` (tests/support/mod.rs:297-340). tests/test_types.py touches enums/statuses only (no backend config assertions).

---

## 5. Side effects + wall clock (what Rust tests must inject)

| method | store writes | other effects |
|---|---|---|
| `decide()` | **none** (reads: running_estimate ×1, finished_since ×2, estimate_capacity ×2 in _unaccounted_fraction + ×1 per _frac_to_tokens call) | reads agent.db via read_windows |
| `keep_fresh()` | `window_log` INSERT per window (always) + conditional `calibration_samples` INSERT — both via `_observe` (facade.py:341,360-398) | spawns `omp usage invalidate` then `omp usage` (facade.py:350-357); reads agent.db |
| `status()` | **none** (reads ledger like decide + estimate_capacity per lid) | reads agent.db |
| `run()` | none itself — scheduler's `_record_job` persists the RunResult (scheduler.py:92-115) | spawns omp worker; two read_windows snapshots; sets rr.usage_delta |

No scheduler wake from the backend: `_wake` (server.py:95) is set by server POST handlers only. Denied-job rows + `deny` events are written by the scheduler callers, never by decide().

Clock call sites (`time.time()`):
- capacity.read_windows capacity.py:89 (expiry/rollover/age_s)
- facade._decide_inner :107 and facade._compute_headroom :186 (two reads inside one decide — a frozen Clock removes intra-call skew)
- facade._observe facade.py:367; facade.status :408 + `time.strftime/localtime` :473-475 (**local timezone**, 12-h clock)
- store `now_ms()` for observed_at in log_window_observation store.py:1488 / record_calibration_sample :1519 and the `resets_at < now` bound in estimate_capacity :1410 (types.py:90-91)
- harness.run_worker harness.py:164,222,241 (Rust: harness.rs — spawn time :250-253 via `SystemTime::now()`, wallclock cap :385-389)
Ramp/retry functions are already pure (take now_ms / resets_at as args) — keep them free functions.

Rust recommendations:
1. `trait Clock { fn now_ms(&self) -> f64 }` (or i64) as a field on the backend AND available to the store. Python tests run against the real clock with relative offsets and ±2000 ms tolerances (test_budget.py:245,296,355); with an injected fixed clock, Rust asserts exact values instead.
2. Window source: `read_windows(db_path, now_ms) -> BTreeMap<String, WindowState>` pure, with the agent.db path a backend field; tests either point the path at a fixture DB (test_budget.py:566) or swap the source wholesale (test_budget.py:117; test_refresh_stale_probe.py:77-80). A `WindowSource` trait (or `fn windows(&self)` overridable seam) covers both.
3. Prober exec: inject the command runner (`trait UsageProber` or a closure field) — tests assert exact argv and call ORDER, and the (124/127, never-panic) rc mapping of util.run_cmd:9-40.
4. status() reset time — **decided against `chrono Local`**. Python's `%I:%M %p` + `lstrip("0")` renders in the *server's* timezone; the shipped `render_status` instead emits `resets <time data-ms="{resets_at}"></time> ({countdown})` (facade.rs:566-585) and the UI formats it in the *viewer's* timezone (StatusPage.svelte:35-45). That is both more correct for a daemon watched from elsewhere and deterministic, which is what lets tests/status_html_test.rs snapshot the whole fragment — a server-local 12-hour clock would make those snapshots machine-dependent. The `<time>` element is therefore the one field `render_status` does **not** html-escape (facade.rs:643), pinned by `limit_id_is_escaped_but_the_time_element_is_not` (status_html_test.rs:202-223).
5. Hold windows in a BTreeMap everywhere: Python dict order (= insertion = SQL group order) decides which :7d window's reason string wins and status() sorts explicitly anyway.

---

## 6. Implementation scope — nothing in this contract is stubbed

The backend, the scheduler and the daemon all landed; there is no seam left
deliberately unimplemented, so this section is a map rather than a plan.

**Trait + data** (`src/backend.rs`): `JobClass`/`Verdict`/`Outlook` (:19-74); `SpendLedger`'s 7 methods (:76-117), implemented on `Store` over the sqlx pool (store.rs:2275-2446, SQL per §1.6); the `Prober` subprocess seam + `CmdProber` (backend.rs:119-135); `Backend` itself (backend.rs:137-166) with all four methods — `decide`, `keep_fresh`, `status_html`, `run`.

**`omp_scavenge`** (`src/backends/omp_scavenge/`): `capacity.rs` (`read_windows`, the `ramp_7d`/`ramp_5h` + `retry_at_*` pair, `effective_used`), `facade.rs` (`impl Backend for OmpScavengeBackend`) including `_usage_snapshot_sync` taken either side of a worker run, and `harness.rs` (`run_worker` spawn+meter, `run_session_dir`/`ledger_dir_usage` for the run's private session directory, `ledger_usage`, and the SIGTERM→SIGKILL group kill `util::kill_tree` (util.rs:66)). `NullBackend` (backend.rs:168-202) remains for router tests only: its `run()` errors by construction, which is the point.

**Callers**: `src/scheduler.rs` has `pick_next`, `anticipated_tokens`, `record_job`, `run_cycle` and every runner (`run_hunt`, `run_recheck`, `run_fix`, `run_engage`, `run_harvest`, plus the repo-level `run_test_gap`/`run_dep_update`/`run_refactor`/`run_modernize`/`run_standards`) and `sync_prs`. `src/daemon.rs` has `run_daemon` (UI server task + usage-prober task + scheduler loop in one process), `acquire_lockfile`, `reconcile_and_log`, `describe_cycle` and `compute_sleep_s`, with `USAGE_PROBE_TICK_S = 60` (daemon.rs:18) probing `keep_fresh()` at startup and each tick (daemon.rs:288-305).

**Wake path**: the loop sleeps on `compute_sleep_s` but races that against its `Notify` and the shared shutdown watch channel (daemon.rs:351-358, latching `stop` from the watch rather than inferring it from the winning branch, :359-361), and sets the `cycle_running` flag around each cycle (daemon.rs:312, :320). POST `/api/cycle` and a mode-setting POST `/api/override` notify it in-process — see API-CONTRACT-WRITES.md §§2,5.

---

## Appendix A — source-vs-brief discrepancies (trust the code)
1. `estimate_capacity` is **not** p75-of-calibration-ratio: the implementation is max-hunter-spend-per-completed-cycle from window_log × jobs (store.py:1397-1434), which is also what the SpendLedger docstring says (backend.py:147). `calibration_samples` is written by `_observe` but **read by no decision path** — informational only, exactly as schema.sql:133-142 says.
2. `_min_delta` in `estimate_capacity` is dead (unreferenced in store.py:1397-1434).
3. Config keys `budget.deny5hAbove` / weekly-reserve don't exist (§3 flag); ramp math replaced them.
4. `WindowState.stale` (capacity.py:57-58) is dead code — nothing references `.stale`; live checks use `cfg.stale_after_s`.
5. ~~`_frac_to_tokens` never consults `estimate_capacity("anthropic:7d")` while `_unaccounted_fraction` does — deliberate-looking asymmetry; port verbatim.~~ **Retracted — this was never true.** `_frac_to_tokens` calls the shared `_capacities()` helper (`facade.py:273`), whose 7d line reads `estimate_capacity("anthropic:7d") or (cap_5h / _5H_7D_RATIO)` (`facade.py:135`) — the same single derivation `_unaccounted_fraction` reaches through `_capacities()` (`facade.py:123`); the ratio is the fallback for when there is no history to estimate from, not the rule. See §"frac_to_tokens" above. Struck rather than deleted because acting on it would change shipped behaviour, and a reader who remembers the claim should find its retraction rather than its absence.
