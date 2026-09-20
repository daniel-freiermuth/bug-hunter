# Hunter read-only API contract (Python -> Rust port, round 1)

Extracted from the Python source at commit state of 2026-09-13. Every claim cites `file:line`.
Sources: `hunter/hunter/server.py` (handler), `hunter/hunter/store.py` (SQL), `hunter/hunter/types.py` (types/config), `hunter/schema.sql` (DDL), `hunter/ui/src/app.ts` (consumer).

Parity bar: the vanilla-TS UI (`hunter/ui/src/app.ts`) must work unchanged. `/api/summary` is zod-validated client-side (app.ts:166-178), so its shape is a **hard** runtime contract; the other endpoints are consumed via plain TS interfaces (soft contract, but every listed key is rendered).

---

## 1. Transport & global behavior

- `ThreadingHTTPServer` subclass `_Server` (server.py:682-691): `allow_reuse_address = True`, `request_queue_size = 64`, `daemon_threads = True` (server.py:709).
- Binds `("127.0.0.1", port or cfg.serve_port)` — **loopback only** (server.py:694-697). Port already in use (errno 98) -> process exits with an error message (server.py:698-706).
- Handler class attrs (server.py:137-142): `server_version = "hunter/1"`, `protocol_version = "HTTP/1.1"` (keep-alive), `timeout = 15` (idle keep-alive connections closed after 15 s).
- Every response goes through `_send` (server.py:157-163) which sets exactly these headers:
  - `Content-Type: <ctype>`
  - `Content-Length: <len>`  (always set; enables keep-alive)
  - `Cache-Control: no-store`  (**on every response, including static files**)
  - plus stdlib defaults `Server: hunter/1 Python/x.y` and `Date`.
- **No CORS headers anywhere.** Same-origin only. (POST additionally requires `Content-Type: application/json`, server.py:409-413 — CSRF mitigation; out of scope here.)
- JSON responses: `_json` = `json.dumps(obj)` with default separators (`, ` / `: `), `Content-Type: application/json` — **no charset parameter** (server.py:165-166).
- Error envelope: `_error(status, message)` -> body `{"error": "<message>"}` as JSON (server.py:168-169).
- Catch-all: any exception inside `do_GET` -> logged, response `500 {"error": "internal error"}` (server.py:247-249). This matters: several "validation" failures surface as 500, not 400 (see /api/findings).
- Unknown GET path -> `404 {"error": "not found"}` (server.py:246).
- Methods other than GET/POST: stdlib `BaseHTTPRequestHandler` default -> `501 Unsupported method` HTML error page. HEAD is NOT implemented.
- One fresh `Store` (own SQLite connection) per request (`_store`, server.py:151-154). No request-level locking for reads.
- Process startup (`serve`, server.py:713-724): exclusive `flock` on `<work_root>/hunter.lock` (server.py:58-81); second instance exits.

---

## 2. Static file serving

Handled first in `do_GET` (server.py:195-210). UI files live in `UI_DIR = PROJECT_ROOT / "ui"` where `PROJECT_ROOT` is the directory containing the `hunter/` package tree, i.e. the repo's `hunter/` dir (types.py:17-20). On-disk contents: `ui/index.html` (checked in), `ui/app.js` + `ui/app.js.map` (tsc output from `ui/src/app.ts`, gitignored). `index.html` references only `app.js` (ui/index.html:446).

| Path | Behavior |
|---|---|
| `/` or `/index.html` | Serve `UI_DIR/index.html` bytes, `200`, `text/html; charset=utf-8`. If missing: `404 {"error": "ui/index.html missing"}` (server.py:195-201). |
| any path NOT starting with `/api/` whose last `.`-suffix is in the type map | Serve `UI_DIR/<path-with-leading-slashes-stripped>` if it is a regular file **and** `UI_DIR in asset.resolve().parents` (path-traversal guard; symlinks resolved) (server.py:203-210). |
| anything else | falls through to `404 {"error": "not found"}` (server.py:246). Note: a non-`/api/` path with an unmapped/absent extension is 404 JSON, and a mapped extension whose file is missing or escapes UI_DIR is also 404 JSON. |

Content-type map `_STATIC_TYPES` (server.py:185-189):

| ext | Content-Type |
|---|---|
| `.js` | `application/javascript; charset=utf-8` |
| `.css` | `text/css; charset=utf-8` |
| `.map` | `application/json` |

No ETag / Last-Modified / Range support; `Cache-Control: no-store` on everything (server.py:161).
Suffix extraction: `url.path.rsplit(".", 1)[-1]` if the path contains a `.`, else no match (server.py:204-205). Query strings are ignored for static paths (urlparse splits them off, server.py:192).

---

## 3. GET /api/summary

Handler: server.py:211-212 -> `_summary()` (server.py:251-326) -> `_validate_summary()` (server.py:852-860).

- **Query params: none** (ignored).
- Success: `200`, a single JSON object of shape `SummaryDict` (server.py:822-849).
- `_validate_summary` runs `pydantic.TypeAdapter(SummaryDict).validate_python(payload)` right before serialization (server.py:850-860). A malformed payload raises -> `500 {"error":"internal error"}`. **Side effect to preserve: pydantic drops keys not declared in the TypedDicts.** Concretely, `repos` rows come from `SELECT *` and would contain the 6 migration-added repo columns (`last_full_hunt_at` … `last_standards_at`) — pydantic strips them, so `/api/summary`'s repo objects carry exactly the 10 `RepoDict` keys, while `/api/repos` (unvalidated) returns all columns. `[INFERENCE]` from pydantic's default TypedDict extra-key behavior; verify with a live diff if in doubt.

### Top-level shape (server.py:305-326, types at server.py:822-849)

| key | type | source |
|---|---|---|
| `backend_status_html` | string | `self.backend.status()` — **this is the key that carries the backend-status HTML fragment**; the UI assigns it via `innerHTML` (server.py:256,306; app.ts:167). Opaque string; Rust port must expose the same key name. |
| `counts` | object: status -> int | `dict.fromkeys(FINDING_STATUSES, 0)` then `Counter` of `status` over `list_all_findings()` (server.py:257-259). **All 9 status keys always present** (zero-filled): `new, rechecking, queued, fixing, pr_open, merged, rejected, wontfix, note` (types.py:23-33,66). |
| `type_counts` | object: type -> int | `Counter` of `type` over the same rows (server.py:261,308). Only observed types present; may be `{}`. |
| `repos` | array of Repo | `store.list_repos()`: `SELECT * FROM repos WHERE deleted_at IS NULL ORDER BY name`, rows through `_repo_row` (store.py:460-464, 101-113), then pydantic-narrowed to `RepoDict` (types.py:143-158): `id:int, name:str, url:str, path:str, forge:str, default_branch:str, last_hunt_sha:str\|null, last_hunt_at:int\|null, enabled:int (0/1, NOT bool), added_at:int`. The narrowing drops the 6 `last_*_at` migration columns here; `deleted_at` is already gone before it (§7). |
| `last_cycle` | Event object or null | first event with `kind == "cycle"` within `recent_events(limit=500)` (server.py:262-265). Event shape: see /api/events. |
| `cycle_running` | bool | `_cycle_lock.locked()` (server.py:304,319) — true JSON boolean. |
| `current_job` | Job object or null | `store.current_job()` (store.py:738-761), see below. |
| `next_candidate` | object or null | computed only when `current_job` is null AND `scheduler.pick_next` yields a candidate (server.py:272-301); see below. |
| `scheduler_state` | object or null | `store.get_scheduler_state()`: `SELECT * FROM scheduler_state WHERE id = 1` (store.py:775-782). Shape (types.py:109-115): `id:1, state:str ("idle"\|"denied"\|"error"), detail:str, next_wake_at:int\|null (epoch ms), updated_at:int`. Null until the daemon loop has run once. |
| `activity_status` | tagged union on `kind` | `_activity_status(...)` (server.py:863-960); see below. |

### current_job (store.py:738-761)

SQL: `SELECT j.*, r.name AS repo_name FROM jobs j JOIN repos r ON r.id = j.repo_id WHERE j.state = 'running' ORDER BY j.id DESC LIMIT 1`.
If `finding_id` is set and the finding exists, two extra keys are added: `finding_summary` (string|null) and `finding_fingerprint` (string|null) — **absent otherwise, not null** (store.py:748-753; types.py:135-141 `NotRequired`).
JobDict keys (types.py:117-141): `id:int, kind:str, repo_id:int, repo_name:str, finding_id:int|null, state:str, pid:int|null, session_file:str|null, cap_tokens:int|null, tokens_new:int|null, calls:int|null, exit_code:int|null, killed_reason:str|null, notes:str|null, started_at:int|null, finished_at:int|null, model:str|null, usage_delta:float|null` (+ the two optional finding_* keys).

### next_candidate (server.py:272-301, TypedDict server.py:764-777)

Built from `scheduler.pick_next(store, cfg)` (exceptions swallowed -> null) and `backend.decide(anticipated_tokens=...)`; verdict = `outlook.prioritized` if the finding has `budget_override` set, else `outlook.normal` (server.py:283-286).

| key | type | source |
|---|---|---|
| `kind` | string | job kind from pick_next |
| `id` | int | target row id |
| `label` | string or null | `target.get("summary") or target.get("name") or target.get("fingerprint")` (server.py:295) |
| `is_finding` | bool | `kind in ("engage","harvest","recheck","fix")` (server.py:280) |
| `is_prioritized` | bool | `bool(budget_override)` (server.py:297) |
| `budget_state` | string | `"denied"` or `"allowed"` — only these two values are ever emitted (server.py:288-291) |
| `budget_reason` | string | `Denied.reason` / `Granted.reason` |
| `budget_retry_at` | number or null | `Denied.retry_at` (epoch ms, float) when denied; always null when allowed (server.py:288-291) |

### activity_status (server.py:863-960; variant TypedDicts server.py:780-819)

Discriminated union on `kind`, priority order:
1. `{"kind":"running","job":<JobDict>}` — when `current_job` non-null
2. `{"kind":"working"}` — `cycle_running` true, no job row yet
3. `{"kind":"error","detail":<scheduler_state.detail>}` — `scheduler_state.state == "error"`
4. `{"kind":"paused","candidate":<NextCandidateDict>}` — candidate present with `budget_state == "denied"`
5. `{"kind":"ready","candidate":<NextCandidateDict>}` — candidate present otherwise
6. `{"kind":"idle"}` — scheduler_state present, none of the above
7. `{"kind":"warming_up"}` — nothing at all (no cycle has ever run)

Variants carry ONLY the listed fields (no `job: null` on non-running variants). The UI parses this with `z.discriminatedUnion` (app.ts:156-164) and an exhaustive switch — new/renamed variants are a breaking change.

---

## 4. GET /api/findings

Handler: server.py:214-215 -> `_findings(qs)` (server.py:340-384).

### Query params (all optional; empty string == absent, server.py:342-346)

| param | type | default | behavior |
|---|---|---|---|
| `status` | string | none | SQL `status = ?`. Not validated against the enum — an unknown value just matches nothing. |
| `repo` | string | none | all-digits -> repo id lookup, else name lookup (`get_repo`, store.py:220-224). Python: **unknown repo raises ValueError -> `500 {"error":"internal error"}`** (server.py:350-355 + 247-249), not 400. **Rust deviation — `400 {"error":"unknown repo <key>"}`** (`ApiError::BadRequest`, server.rs:333): a bad query param is the caller's mistake, and a 500 both lies about whose fault it is and hides the reason behind the generic body. |
| `severity` | string | none | minimum severity; case-insensitive `low\|medium\|high` (types.py:44-63). Expands to `severity IN (...)` of all values at-or-above. Python: **invalid value -> ValueError -> 500** (same catch-all). **Rust deviation — `400 {"error":"invalid severity <value>"}`** (server.rs:337-338), same reasoning. |
| `type` | string | none | SQL `type = ?` on `findings.type`. |
| `unified` | string | `"1"` | `"1"` (default) -> `list_all_findings` (adds `category`); any other value -> legacy `list_findings` (no `type` filter applied, no `category` key) (server.py:346,357-371). The UI never sends it. |

### Response: `200`, JSON array (no pagination, no LIMIT)

Store: `list_all_findings` (store.py:397-441): `SELECT * FROM findings [WHERE status = ? / repo_id = ? / type = ? / severity IN (…)] ORDER BY id DESC`. Post-processing adds a computed `category` key per row (store.py:432-441):

| row `type` | `category` |
|---|---|
| `bug` | `bug_class` value |
| `dep_update` | `update_type` value |
| `test_gap` | literal `"coverage"` |
| `refactor` | `smell_type` value |
| `modernization` | `modernization_class` value |
| `standards` | `standard_section` value — Rust-only branch (`Finding::category()`, types.rs:136-145); the Python mapping has no `standards` case |
| anything else | **key absent** |

(Legacy `list_findings`, store.py:372-395: same SELECT minus the `type` cond and minus `category`.)

Handler then embeds per-row (server.py:372-384):
- `timeline`: **always added**, array of event rows for that finding, ascending id order. Store: `events_by_finding(fids)` (store.py:861-877): `SELECT * FROM events WHERE finding_id IN (…) ORDER BY id`, grouped by `finding_id`. Empty array when no events.
- `needs_attention`: added **only** for rows with `status == "pr_open"` that have a `pr_state` row (`get_pr_state`, store.py:613-615); value is `pr_state.needs_attention` (string or null). Key **absent** for all other rows.

### Row columns (findings table verbatim; schema.sql:18-66 + migrations, see section 10)

`id:int, type:str, repo_id:int, fingerprint:str, file:str|null (in practice "" default at insert, store.py:341), symbol:str|null, line:int|null, severity:str ("high"|"medium"|"low"), confidence:float, summary:str, detail:str|null, status:str, pr_url:str|null, created_at:int, updated_at:int, bug_class:str|null, evidence_plan:str|null, introduced_by:str|null, rung_achieved:int|null, verdict_reason:str|null, budget_override:str|null ("once"|"exempt"|null), fix_attempts:int, last_fix_failure:str|null, recheck_attempts:int, last_recheck_failure:str|null, ecosystem:str|null, package:str|null, current_version:str|null, latest_version:str|null, update_type:str|null, security_advisory:str|null, missing_tests:str|null (JSON-encoded array stored as TEXT — served as a string, NEVER json.loads'd), test_file:str|null, smell_type:str|null, suggested_refactor:str|null, modernization_class:str|null, current_approach:str|null, proposed_approach:str|null, standard_section:str|null` + computed `category`, `timeline`, conditional `needs_attention`.

---

## 5. GET /api/finding

Handler: server.py:217-223 -> `_finding_detail(qs)` (server.py:386-407).

- Query param `id`: required, must be all digits. Missing, non-digit, or nonexistent finding -> **`404 {"error": "no such finding"}`** in all three cases (server.py:219-221, 397-402). (Never 400, despite the docstring.)
- Success `200`:
```json
{ "jobs": [ <job row>, ... ], "pr_state": { ... } | null }
```
- `jobs`: `jobs_by_finding(fid)` (store.py:723-736): `SELECT j.*, r.name AS repo_name FROM jobs j JOIN repos r ON r.id = j.repo_id WHERE j.finding_id = ? ORDER BY j.id DESC` — complete history, **no limit**. Columns: the 17 jobs columns (section 6) + `repo_name`.
- `pr_state`: `get_pr_state(fid)` (store.py:613-615): `SELECT * FROM pr_state WHERE finding_id = ?`; the whole row as an object, or `null` if none. Columns (schema.sql:135-176): `finding_id:int, pr_number:int|null, state:str|null (OPEN|MERGED|CLOSED), mergeable:str|null, checks:str|null, head_ref:str|null, last_activity_at:int|null, last_engaged_activity_at:int|null, needs_attention:str|null, attention_since:int|null, attention_fingerprint:str|null, addressed_fingerprint:str|null, head_sha:str|null, addressed_head_sha:str|null, synced_at:int|null, harvested_at:int|null, harvest_attempts:int, last_harvest_failure:str|null`.

---

## 6. GET /api/jobs

Handler: server.py:224-226. No query params (ignored).

Store: `list_jobs(limit=50)` (store.py:713-721):
```sql
SELECT j.*, r.name AS repo_name FROM jobs j
 JOIN repos r ON r.id = j.repo_id
 ORDER BY j.id DESC LIMIT 50
```
Response: `200`, JSON array. Row = all 17 `jobs` columns + `repo_name:str`:
`id:int, kind:str, repo_id:int, finding_id:int|null, state:str (queued|running|done|failed|killed|denied), pid:int|null, session_file:str|null, cap_tokens:int|null, tokens_new:int|null, calls:int|null, exit_code:int|null, killed_reason:str|null, notes:str|null, model:str|null, usage_delta:float|null, started_at:int|null, finished_at:int|null` (schema.sql:73-93).
Note: jobs whose repo row was deleted would vanish (INNER JOIN) — moot in practice because `delete_repo` refuses while jobs reference the repo (store.py:246-263).

---

## 7. GET /api/repos

Handler: server.py:231-232; Rust `repos` -> `Json<Vec<Repo>>` (server.rs:408-410). No query params.
Store: `list_repos()` — Python `SELECT * FROM repos WHERE deleted_at IS NULL ORDER BY name` with each row passed through `_repo_row` (store.py:460-464, helper at store.py:101-113); Rust `Store::list_repos` selects an explicit column list with the same `WHERE deleted_at IS NULL ORDER BY name` (store.rs:683-698). `ORDER BY name` is the default BINARY collation, so it is case-sensitive.
Response: `200`, JSON array of repos rows — schema columns `id:int, name:str, url:str, path:str, forge:str (github|gitlab), default_branch:str, last_hunt_sha:str|null, last_hunt_at:int|null, enabled:int (0|1), added_at:int` (schema.sql:5-18) **plus** the 6 migration columns `last_full_hunt_at:int|null, last_test_gap_at:int|null, last_dep_update_at:int|null, last_refactor_at:int|null, last_modernization_at:int|null, last_standards_at:int|null` (schema.sql:19-24; §11). Unlike `/api/summary.repos`, nothing strips *those*.

**`deleted_at` is not part of this shape**, in either daemon. The column exists on the table (schema.sql:25, §11) but it is bookkeeping for two-phase deletion, not API surface: Rust never selects it (`Repo` has no such field, types.rs:29-49), and Python's `SELECT *` would otherwise leak it, so `_repo_row` pops it (store.py:101-113). The filter makes the value uninteresting anyway — every repo read path is `WHERE deleted_at IS NULL`, so a flagged repo is absent from this array entirely rather than present with a timestamp, and the key could only ever have serialized as `null`. Stripping it keeps a rollback from changing the response shape. The same applies to `/api/repo`'s and `POST /api/repos`' embedded `repo` object, which come from `get_repo` through the same helper (WRITES §§6-7).

`repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (schema.sql:9; hunter-rs migration 009, which rebuilds the table because AUTOINCREMENT cannot be added by ALTER TABLE, 009_repos_autoincrement.sql:32, :68-69). Ids therefore come from `sqlite_sequence` and only ever move forward: a freed id is never handed out again, so a client still holding a stale one — a browser tab left open across the deletion — can only ever *miss*. It gets `404 {"error": "no repo <id>"}` from `/api/repo` and `/api/repo/notes`, and from `/api/repo/delete` either a `404` or, while the row is still flagged, an idempotent `{"ok": true}` that re-attempts reclamation (server.py:669-682; server.rs:1086-1100). What it can no longer do is pause, delete or annotate whichever repo would otherwise have inherited the number — a window that ordering the cleanup cannot close, because it is as long as the tab stays open (009_repos_autoincrement.sql:2-21). Two-phase deletion (WRITES §8) stays: it is what stops a half-removed directory being mistaken for a clone.

---

## 8. GET /api/repo/notes

Handler: server.py:230-232 -> `_repo_notes(qs)` (server.py:328-338).

| condition | response |
|---|---|
| `id` missing or not all-digits | `400 {"error": "id query param must be an integer"}` (server.py:330-332) |
| no repo with that id | `404 {"error": "no repo <id>"}` (server.py:334-336) |
| ok | `200 {"notes": "<string>"}` (server.py:338) |

Notes are **not in the DB**: read from `<work_root>/repos/repo-<id>/NOTES.md` (`repo_notes_path`, store.py:269-276). Missing file -> `""`. Files longer than 4000 chars are truncated to the **tail** 4000 chars with prefix `"...(older notes truncated)...\n"` (`repo_notes`, store.py:278-294, `_MAX_NOTES_CHARS = 4000`).

---

## 9. GET /api/events

Handler: server.py:233-235.
Store: `recent_events(limit=100)` (store.py:852-859): `SELECT * FROM events ORDER BY id DESC LIMIT 100`.
Response: `200`, JSON array of `{id:int, at:int (epoch ms), kind:str, message:str, job_id:int|null, finding_id:int|null}` (schema.sql:125-132; types.py:161-168).

---

## 10. GET /api/stats

Handler: server.py:236-245 — the response is the **complete** three-key object:
```json
{ "totals": {...}, "by_kind": [...], "by_finding": [...] }
```

### totals — `stats_totals()` (store.py:1062-1075), single object
```sql
SELECT COUNT(*) AS jobs,
 SUM(tokens_new) AS total_tokens,
 SUM(calls) AS total_calls,
 SUM(usage_delta) AS total_usage_delta,
 SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,
 SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied
FROM jobs
```
Keys: `jobs:int, total_tokens:int|null, total_calls:int|null, total_usage_delta:float|null, done:int|null, denied:int|null`. With zero job rows: `jobs = 0` and every SUM is null (SQL semantics) — the UI declares `done`/`denied` as `number` (app.ts:216-221) but only formats them, so null survives; replicate SQL behavior exactly. (`rows[0] if rows else {}` — the else branch is unreachable for an aggregate.)

### by_kind — `stats_by_kind()` (store.py:1028-1044), array
```sql
SELECT kind, COUNT(*) AS jobs,
 SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,
 SUM(CASE WHEN state='failed' THEN 1 ELSE 0 END) AS failed,
 SUM(CASE WHEN state='killed' THEN 1 ELSE 0 END) AS killed,
 SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied,
 SUM(tokens_new) AS total_tokens,
 SUM(calls) AS total_calls,
 AVG(tokens_new) AS avg_tokens,
 SUM(usage_delta) AS total_usage_delta,
 GROUP_CONCAT(DISTINCT model) AS models
FROM jobs GROUP BY kind ORDER BY kind
```
Row keys/types: `kind:str, jobs:int, done:int, failed:int, killed:int, denied:int, total_tokens:int|null, total_calls:int|null, avg_tokens:float|null, total_usage_delta:float|null, models:str|null` (comma-joined distinct model strings, e.g. `"opus,sonnet"`).

### by_finding — `stats_by_finding()` (store.py:1046-1060), array
```sql
SELECT j.finding_id, f.fingerprint, f.status, f.severity,
 COUNT(*) AS jobs,
 SUM(j.tokens_new) AS total_tokens,
 SUM(j.calls) AS total_calls,
 SUM(j.usage_delta) AS total_usage_delta
FROM jobs j JOIN findings f ON f.id = j.finding_id
WHERE j.finding_id IS NOT NULL
GROUP BY j.finding_id
ORDER BY total_tokens DESC
```
Row keys: `finding_id:int, fingerprint:str, status:str, severity:str, jobs:int, total_tokens:int|null, total_calls:int|null, total_usage_delta:float|null`. Note `ORDER BY total_tokens DESC` puts NULL totals last in SQLite (NULLs sort last on DESC).

---

## 11. Database DDL (schema.sql + in-code migrations)

`Store.__init__` (store.py:148): executes `schema.sql` as a script on every connection open (all `CREATE TABLE/INDEX IF NOT EXISTS`), then probes-and-applies these ALTERs for pre-existing DBs (list and loop at store.py:156-262; probe = `SELECT <col> FROM <tbl> LIMIT 1`, on OperationalError run the ALTER):

```sql
ALTER TABLE repos ADD COLUMN forge TEXT NOT NULL DEFAULT 'github';           -- also in schema.sql
ALTER TABLE jobs ADD COLUMN model TEXT;                                       -- also in schema.sql
ALTER TABLE jobs ADD COLUMN usage_delta REAL;                                 -- also in schema.sql
ALTER TABLE findings ADD COLUMN budget_override TEXT;                         -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_full_hunt_at INTEGER;                       -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_test_gap_at INTEGER;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_dep_update_at INTEGER;                      -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_refactor_at INTEGER;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_modernization_at INTEGER;                   -- also in schema.sql
ALTER TABLE findings ADD COLUMN modernization_class TEXT;                     -- also in schema.sql
ALTER TABLE findings ADD COLUMN current_approach TEXT;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN deleted_at INTEGER;                              -- also in schema.sql
ALTER TABLE findings ADD COLUMN proposed_approach TEXT;                       -- also in schema.sql
ALTER TABLE findings ADD COLUMN standard_section TEXT;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_standards_at INTEGER;                       -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN harvested_at INTEGER;                         -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN attention_since INTEGER;                      -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN attention_fingerprint TEXT;                   -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN addressed_fingerprint TEXT;                   -- also in schema.sql
ALTER TABLE findings ADD COLUMN fix_attempts INTEGER NOT NULL DEFAULT 0;      -- also in schema.sql
ALTER TABLE findings ADD COLUMN last_fix_failure TEXT;                        -- also in schema.sql
ALTER TABLE findings ADD COLUMN recheck_attempts INTEGER NOT NULL DEFAULT 0;  -- also in schema.sql
ALTER TABLE findings ADD COLUMN last_recheck_failure TEXT;                    -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN harvest_attempts INTEGER NOT NULL DEFAULT 0;  -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN last_harvest_failure TEXT;                    -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN head_sha TEXT;                                -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN addressed_head_sha TEXT;                      -- also in schema.sql
```

Every column in that list is *also* declared in `schema.sql`, so a fresh Python
install and an upgraded one converge on the same set of columns. The ALTERs
still matter: `schema.sql` runs `CREATE TABLE IF NOT EXISTS`, which does
nothing to a table that already exists, so on a pre-existing database the ALTER
list is the only thing that adds them. The Rust port's migrations must cover
both paths — `hunter/tests/test_schema_parity.py` asserts the two systems agree
from a fresh install *and* from an upgrade.

Two things about `repos` do **not** follow that pattern:

- The partial index `repos_deleted_at ON repos(deleted_at) WHERE deleted_at IS NOT NULL` is **not** in `schema.sql`, and cannot be: `schema.sql` runs before the ALTERs, and on a pre-existing database `repos.deleted_at` does not exist yet at that point. Python creates it after the ALTER loop instead (store.py:291-298); hunter-rs creates it in migration 008 next to the column (008_repos_deleted_at.sql:21, :27-28) and again after the rebuild in 009, because dropping a table drops its indexes (009_repos_autoincrement.sql:104-105).
- `repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` in `schema.sql` (:9), but AUTOINCREMENT cannot be added by an ALTER — it needs a table rebuild (009_repos_autoincrement.sql:32), and the probe-and-ALTER list deliberately has none. So this one does **not** converge on its own: a fresh Python install gets it from `schema.sql`, an existing database gets it from hunter-rs migration 009, and a Python-only installation that predates 009 keeps plain rowid-alias ids — which reuse a deleted repo's id, with the consequences §7 describes. A rebuild sitting in the probe-and-ALTER list would be a much riskier thing than the ALTERs around it, and the deployed database is migrated by the Rust daemon, which is the rollback path that matters here.

Also: `DROP INDEX IF EXISTS findings_fingerprint` cleanup on every open (store.py:269-270). PRAGMAs: `journal_mode = WAL`, `foreign_keys = ON` (schema.sql:2-3).

Tables (schema.sql): `repos` (:5-26), `findings` (:28-83 + 5 indexes :84-88), `jobs` (:90-109 + 2 indexes :110-112), `window_log` (:115-123), `calibration_samples` (:135-142), `events` (:144-151 + index :152), `pr_state` (:156-199), `scheduler_state` (:206-212, single row `CHECK (id = 1)`).

**There is no `repo_notes` table** — repo notes are Markdown files under `<work_root>/repos/repo-<id>/NOTES.md` (store.py:571-578).

Tables the read-only API touches: `findings`, `repos`, `jobs`, `events`, `pr_state`, `scheduler_state`. (`window_log`/`calibration_samples` are backend-internal.)

---

## 12. Config the serve path needs (`hunter/config.json`, loaded by `Config.load`, types.py:199-228)

Paths are resolved relative to `PROJECT_ROOT` (the `hunter/` dir containing the package) when not absolute (types.py:204-207).

| config key | Config field | default | used by serve path for |
|---|---|---|---|
| `serve.port` | `serve_port` | `8377` (types.py:187,223) | listen port; bind host is hardcoded `127.0.0.1` (server.py:697) |
| `workRoot` | `work_root` | `"data"` | `hunter.lock` lockfile (server.py:70), `repos/repo-<id>/NOTES.md` (store.py:276) |
| `dbPath` | `db_path` | `"data/hunter.db"` | SQLite database (store.py:116-117) |
| — (constant) | `UI_DIR` | `PROJECT_ROOT / "ui"` (types.py:20) | static files + index.html |

Actual config.json in this repo: `workRoot=data`, `dbPath=data/hunter.db`, `serve.port=8377` (hunter/config.json). Other keys (hunt/fix caps, models, budget) feed the scheduler/backend, which `/api/summary` reaches only through `scheduler.pick_next` / `backend.decide()` / `backend.status()` — for round 1 those can stay behind a trait boundary.

---

## 13. UI consumer cross-check (hunter/ui/src/app.ts)

### Every fetch the UI makes

| call site | request | validation |
|---|---|---|
| app.ts:1344 | `GET /api/summary` (5s poll) | **zod `SummarySchema.parse` — hard contract** (app.ts:166-178) |
| app.ts:1345 | `GET /api/findings` (no query params — filtering is client-side) | TS interface only |
| app.ts:1346 | `GET /api/jobs` | TS interface only |
| app.ts:1347 | `GET /api/events` | TS interface only |
| app.ts:1348 | `GET /api/stats` | TS interface only |
| app.ts:888 | `GET /api/finding?id=<id>` (lazy, on card expand) | TS interface only |
| app.ts:821 | `GET /api/repo/notes?id=<id>` (lazy) | reads `notes`, `error` |
| app.ts:640,654,673,694,710,722,734,746,774,796,858 | POST endpoints (out of scope, section 14) | reads `error`, plus 409 status for /api/cycle |

### Keys the UI actually reads — the port MUST NOT drop any of these

**`/api/summary` (zod = request fails visibly if any is missing/wrong-typed):** `backend_status_html` (string; innerHTML'd), `counts` (record str->num), `type_counts`, `repos[]` (all 10 RepoDict keys required per RepoSchema app.ts:105-116), `last_cycle` (nullable Event), `cycle_running` (bool), `current_job` (nullable; requires JobSchema keys: `id, kind, repo_id, repo_name, finding_id, state, tokens_new, calls, exit_code, killed_reason, started_at, finished_at`; `finding_summary`/`finding_fingerprint` optional; `pid`/`session_file`/`cap_tokens`/`notes`/`model`/`usage_delta` are NOT required by zod — but keep them for /api/jobs parity), `next_candidate` (nullable; requires `kind, id, label, is_finding, budget_state, budget_reason, budget_retry_at`; `is_prioritized` optional-with-default), `scheduler_state` (nullable; requires `state, detail, next_wake_at, updated_at`), `activity_status` (discriminated union; unknown `kind` = parse failure).

**`/api/findings` (interface app.ts:18-56):** `type, id, repo_id, fingerprint, file, symbol, line, category, severity, confidence, summary, detail, evidence_plan, introduced_by, status, verdict_reason, pr_url, created_at, updated_at, timeline (Event[]), budget_override, needs_attention`, plus optional type-specific: `bug_class, ecosystem, package, current_version, latest_version, update_type, security_advisory, missing_tests, smell_type, suggested_refactor, modernization_class, current_approach, proposed_approach, standard_section` (`standard_section` only in the Svelte consumer, ui-svelte/src/lib/types.ts:52).

**`/api/jobs` (JobSchema-derived type, app.ts:58-71):** `id, kind, repo_id, repo_name, finding_id, state, tokens_new, calls, exit_code, killed_reason, started_at, finished_at`.

**`/api/events` (app.ts:73-82):** `id, at, kind, message, job_id, finding_id`.

**`/api/finding` (app.ts:102-105):** `jobs` (Job[]), `pr_state` (nullable; keys read app.ts:86-96: `pr_number, state, mergeable, checks, head_ref, last_activity_at, last_engaged_activity_at, needs_attention, synced_at`).

**`/api/stats` (app.ts:188-223):** `totals.{jobs,total_tokens,total_calls,total_usage_delta,done,denied}`; `by_kind[].{kind,jobs,done,failed,killed,denied,total_tokens,total_calls,avg_tokens,total_usage_delta,models}`; `by_finding[].{finding_id,fingerprint,status,severity,jobs,total_tokens,total_calls,total_usage_delta}`.

**`/api/repo/notes`:** `notes` (string), `error` (on non-200).

**Error handling:** the UI reads `body.error` from failed responses across the board — keep the `{"error": string}` envelope and the specific statuses (400/404/409/415/500).

---

## 14. Appendix: POST endpoints (out of scope for round 1)

All under `do_POST` (server.py:409-441); all require `Content-Type: application/json` else `415`:
`POST /api/verdict` (set finding status), `POST /api/cycle` (trigger cycle; 202/409-busy), `POST /api/recheck`, `POST /api/unqueue`, `POST /api/override` (budget override incl. `id:"all"` clear), `POST /api/repo` (update), `POST /api/repos` (add, 201), `POST /api/repo/delete`, `POST /api/repo/notes` (append note, 201).
