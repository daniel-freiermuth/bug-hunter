# Hunter write-path API contract (Python -> Rust port, round 2)

Extracted from the Python source at commit state of 2026-09-13. Every claim cites `file:line`.
Sources: `hunter/hunter/server.py` (handlers), `hunter/hunter/store.py` (SQL), `hunter/hunter/types.py` (status enums), `hunter/hunter/forge.py` (forge detection), `hunter/hunter/scheduler.py` (deferred clone), `hunter/schema.sql` (DDL), `hunter/ui-svelte/src/` (consumer — the vanilla `hunter/ui/src/app.ts` this document was first written against no longer exists), `hunter/tests/` (behavioral pins).

Shared conventions (error envelope `{"error": "<msg>"}`, `_send` headers, Row shapes, per-request `Store`) are in `hunter-rs/API-CONTRACT.md` sections 1-2 and are not repeated here except where write-specific.

---

## 0. Shared POST plumbing

### 0.1 Content-Type gate (CSRF guard) — runs before everything

`do_POST` (server.py:409-449) first reads `Content-Type` (default `''`) and requires `ctype.startswith('application/json')` (server.py:411-412). Failure -> `415 {"error": "Content-Type must be application/json"}` (server.py:413). Notes:

- **Prefix match**: `application/json; charset=utf-8` passes.
- The gate applies to **all 9 routes including `/api/cycle`**, which never reads a body.
- **Every UI POST clears the gate**: the Svelte UI routes all of them through one `post()` helper that sets `Content-Type: application/json` and a JSON body (`ui-svelte/src/lib/api.svelte.ts:44-50`); the bare `api()` wrapper sets no headers and is GET-only (`api.svelte.ts:32-41`). The body-less `/api/cycle` call is `post("/api/cycle", {})` (`ui-svelte/src/pages/StatusPage.svelte:12`), i.e. it sends `{}` and passes. (This supersedes the old vanilla-`app.ts` discrepancy where `runCycle` sent no header and got a 415.)

### 0.2 Route dispatch and error taxonomy

Dispatch is exact-path string equality on `urlparse(self.path).path` (server.py:415-443); unknown path -> `404 {"error": "not found"}` (server.py:444). The whole dispatch runs inside:

```python
except (ValueError, json.JSONDecodeError) as exc:   # server.py:445-446
    self._error(400, str(exc))
except Exception:                                    # server.py:447-449
    log.exception(...); self._error(500, "internal error")
```

Body parsing `_body_json` (server.py:171-181): reads exactly `Content-Length` bytes (no chunked support; missing/0 CL == empty).

| Condition | Exception | Response |
|---|---|---|
| empty body (no bytes) | `ValueError("empty body")` (server.py:174-176) | `400 {"error": "empty body"}` |
| malformed JSON | `json.JSONDecodeError` | `400 {"error": "Expecting value: line 1 column 1 (char 0)"}` (or whichever decode message) |
| valid JSON, not an object (array/string/number) | `TypeError("body must be a JSON object")` (server.py:178-180) | **`500 {"error": "internal error"}`** — TypeError is NOT caught by the 400 branch |
| `store.delete_repo` refusal (see §8) | `ValueError` from store.py:258-264 | `400` with the store's message |
| non-string truthy `reason` on `/api/verdict` | `AttributeError` (`.strip()` on a non-str, server.py:461 — no `isinstance` guard there) | `500 {"error": "internal error"}` |

A non-string `name` on `/api/repos` used to belong in that row and no longer does: `_add_repo` guards with `raw.strip() if isinstance(raw, str) else ""` (server.py:632-633), so `{"name": 123}` becomes the empty string and is refused by the name rule. Both daemons answer `400 {"error": "invalid repo name"}` — Python because the empty string fails the regex (server.py:635-637), Rust because `as_str` on a non-string yields `""` before `valid_repo_name` ever sees it (`add_repo`, server.rs:1000-1008; `valid_repo_name` itself at server.rs:980-987). Observed on the Python side: `{"name": 123}`, `{"name": {"a": 1}}` and `{"name": null}` each return exactly `b'{"error": "invalid repo name"}'` with status 400. The two daemons agree on status and on the JSON document, but **not byte-for-byte**: `json.dumps` writes `{"error": "invalid repo name"}` with a space after the colon and serde_json writes the compact `{"error":"invalid repo name"}` (observed live: the Rust daemon answers an unknown route with `{"error":"not found"}`). Compare parsed JSON in tests, never raw bytes.

**Python quirk to decide on**: `isinstance(True, int)` is `True`, so JSON `true`/`false` passes every `isinstance(fid, int)` check and is treated as id 1/0. The Rust port should accept only actual integers; no client sends booleans — every UI id comes from `finding.id` / `repo.id`, typed `number`.

### 0.3 Transaction model

- One fresh `Store` per request (`_store`, server.py:151-154) — a new SQLite connection that **re-runs `schema.sql` via `executescript` plus ~25 `ALTER TABLE` migration probes on every construction** (store.py:109-200). Python's sqlite3 default busy timeout is 5 s `[INFERENCE from stdlib default]`; WAL mode (schema.sql:2).
- **Every store method commits itself** (e.g. store.py:216, 248, 265, 472, 509, 591, 849). There are no multi-statement transactions: a handler doing `set_status` + `log_event` performs **two independent commits**; a crash between them loses the event but keeps the status change. The Rust port may keep this (parity) — do not wrap handler bodies in one transaction, it would change observable failure behavior only, which is acceptable, but sqlite row visibility mid-handler is identical either way.
- `updated_at` on `findings` is set to `now_ms()` (epoch **milliseconds**, types.py:86-87) by every findings write (store.py:454, 507, 588). `repos` has **no** `updated_at` column (schema.sql:5-16); repo updates touch nothing but the named columns.

### 0.4 Status vocabulary (types.py)

- `FINDING_STATUSES` = all of `Status` (types.py:66): `new, rechecking, queued, fixing, pr_open, merged, rejected, wontfix, note` (types.py:23-33).
- `VERDICT_STATUSES` = `queued, rejected, wontfix, note, merged` (types.py:75-81).
- `REASON_REQUIRED` = `rejected, wontfix` (types.py:82).

Transition legality as enforced by the server:

| Endpoint | Precondition on current status | New status | Reason |
|---|---|---|---|
| `/api/verdict` | **none** — any current status, including `fixing`/`pr_open` | any of `VERDICT_STATUSES` | mandatory iff new status in `rejected, wontfix`; optional otherwise |
| `/api/recheck` | exactly `new` | `rechecking` | n/a |
| `/api/unqueue` | exactly `queued` | `new` | n/a |

`store.set_status` additionally validates membership in `FINDING_STATUSES` and raises `ValueError(f"invalid status: {status}")` (store.py:449-451) — unreachable from these handlers since `VERDICT_STATUSES ⊂ FINDING_STATUSES`.

---

## 1. POST /api/verdict — human triage verdict

Handler `_verdict` (server.py:451-480).

**Request**: `{"id": int, "status": str, "reason"?: str}`. `reason` is normalized `(body.get("reason") or "").strip() or None` (server.py:455) — absent, null, empty, or whitespace-only all become None.

**Validation order** (first failure wins):
1. `id` not an int -> `400 {"error": "id must be an integer"}` (server.py:456-458).
2. `status not in VERDICT_STATUSES` -> `400` with `f"status must be one of {list(VERDICT_STATUSES)}"` (server.py:459-464). Because `Status` is a `StrEnum`, the f-string renders **enum reprs**, byte-exact: `status must be one of [<Status.QUEUED: 'queued'>, <Status.REJECTED: 'rejected'>, <Status.WONTFIX: 'wontfix'>, <Status.NOTE: 'note'>, <Status.MERGED: 'merged'>]` `[INFERENCE from CPython StrEnum repr; UI only displays the string]`. String comparison works because StrEnum members `==` their values.
3. `status in REASON_REQUIRED and not reason` -> `400 {"error": "reason required for status 'rejected'"}` (repr-quoted status, server.py:465-467).
4. `get_finding(fid)` is None -> `404 {"error": "no finding <fid>"}` (server.py:469-472). `get_finding` = `SELECT * FROM findings WHERE id = ?` (store.py:368-370).

**Writes** (server.py:473-479):
1. `set_status(fid, status, verdict_reason=reason)` -> `UPDATE findings SET status = ?, updated_at = ?[, verdict_reason = ?] WHERE id = ?` + commit (store.py:443-473). `verdict_reason` is only included when reason is non-None — **a verdict without reason does NOT clear a previously stored verdict_reason**.
2. `log_event("verdict", f"finding {fid} [{finding['fingerprint']}] -> {status}" + (f": {reason}" if reason else ""), finding_id=fid)` -> `INSERT INTO events (at, kind, message, job_id, finding_id) VALUES (?,?,?,?,?)` + commit (store.py:839-850). `fingerprint` is from the row read **before** the update.

**Success**: `200 {"ok": true, "finding": <full refreshed row>}` — re-fetched via `get_finding(fid)` (server.py:480), i.e. `SELECT *` on findings: all columns incl. migration-added ones (see round-1 contract's Finding row shape).

**UI**: two callers, both sending `{id, status}` plus `reason` only when non-empty. `FindingCard.doVerdict` (`components/FindingCard.svelte:46-64`) passes `reason: reason || undefined`, which `JSON.stringify` drops; `KanbanPage.verdict` (`pages/KanbanPage.svelte:55-71`) spreads `...(reason ? {reason} : {})`. Neither reads the embedded finding — success is `await store.refresh()`. Failure is `!r.ok` (never a status comparison): `console.error` with the status, and **no user-visible message on this endpoint** — the card simply stays as it was, and in `FindingCard` the reason prompt and its text are deliberately kept so the verdict can be retried (:50-53). Reason for `rejected`/`wontfix` is required client-side in both: `FindingCard` renders an inline input whose Confirm button is `disabled={busy || !reasonText.trim()}` (:192-211), `KanbanPage` uses `prompt()` and calls `verdict` only `if (reason)` (:165-168). `KanbanPage` additionally guards concurrency with a `pending` set keyed by finding id, since the two verdict buttons write the same row with an unconditional UPDATE (:53-57).

---

## 2. POST /api/cycle — manual cycle trigger

Handler `_cycle` (server.py:482-506). **Never reads the body** (only the §0.1 gate applies).

**Flow**:
1. `_cycle_lock.acquire(blocking=False)` — module-global `threading.Lock` (server.py:88). Held -> `409 {"error": "busy"}` via `_json` (server.py:483-485).
2. Else spawn `threading.Thread(target=run, name="hunter-cycle", daemon=True)` (server.py:505) and respond **immediately** `202 {"started": true}` (server.py:506).
3. The thread (server.py:489-503): fresh `Store(cfg)`; `_reconcile_and_log(store)` (server.py:113-133); `scheduler.run_cycle(store, cfg, backend=self.backend)`; on any exception logs + best-effort `log_event("error", "cycle failed (see logs)")` (server.py:498-501); `finally: _cycle_lock.release()` (server.py:502-503).

**State it mutates / needs from in-process state**:
- `_cycle_lock` — shared with the daemon loop (`daemon()` acquires it non-blocking each wake, server.py:1055, releases 1083) and read by `GET /api/summary` as `cycle_running = _cycle_lock.locked()` (server.py:304). This is what makes manual and timed cycles mutually exclusive and what drives the `"working"` activity status (server.py:903-906).
- `Handler.backend` — the singleton `Backend` built once per process in `serve()`/`daemon()` from `cfg.make_backend(ThreadLocalLedger(cfg))` (server.py:713-717, 1043; types.py:239-251).
- Reconcile writes (store.py:783-838): findings stuck at `'fixing'` -> `set_status(id, "queued")` (store.py:823-827); jobs stuck at `'running'` -> `update_job(state="killed", killed_reason="orphaned", finished_at=now_ms(), notes="reconciled at cycle startup -- prior process died mid-job")` (store.py:829-836; `update_job` store.py:704-711). Each touched row also gets a `log_event("error", ...)` with the exact formats at server.py:120-133 (`f"reconciled #{f['id']} stuck 'fixing' -> 'queued' -- prior process died mid-fix"` / `f"reconciled orphaned {r['kind']} job #{r['id']} (finding {r.get('finding_id')}) -- prior process died mid-job"`).
- `run_cycle` itself spawns workers, writes jobs/findings/pr_state, shells out to git/gh — the entire scheduler.

**Rust implementation** (superseded the forwarding proxy this section originally specified): the proxy was only sound while the Python process owned `_cycle_lock`, `_wake` and the backend singleton. Since the scheduler moved into the Rust daemon, those live here, and forwarding to a process that no longer exists made every trigger a 502. `AppState.scheduler: SchedulerHandle` now carries the loop's `running: Arc<AtomicBool>` and its `wake: Arc<Notify>`. The handler checks `running` (409 `{"error":"busy"}`), otherwise notifies and returns 202 `{"started": true}`. The busy check is advisory, as Python's non-blocking `acquire` was: a trigger racing the loop into its next cycle is held as a notify permit and runs on the following iteration instead of being dropped. `cycle_running` in `GET /api/summary` and the `"working"` activity status (§3 priority 2) read the same flag.

**UI** (`StatusPage.runCycle`, `ui-svelte/src/pages/StatusPage.svelte:9-29`): `post("/api/cycle", {})` — header and `{}` body per §0.1, so the gate passes and the real status codes are reached. This is the one caller that compares `status` to literals rather than reading `r.ok`, because here the code *is* the information: 409 -> button text "busy", 202 -> "started…", anything else -> "error" (:13-17); then `store.refresh()`. The button is re-armed after 2.5 s in `finally`, so it recovers even when the request never reached the server (:22-28).

---

## 3. POST /api/recheck — queue a `new` finding for recheck

Handler `_recheck` (server.py:508-527).

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:511-513).
2. no finding -> `404 {"error": "no finding <fid>"}` (server.py:515-518).
3. `finding["status"] != "new"` -> `400` with `f"finding #{fid} is {finding['status']!r}, not 'new'"` — e.g. `finding #7 is 'queued', not 'new'` (server.py:519-523).

**Writes**: `set_status(fid, "rechecking")` (SQL as §1, no verdict_reason); `log_event("recheck", f"#{fid} queued for recheck", finding_id=fid)` (server.py:525-526).

**Success**: `200 {"queued": true, "finding": <refreshed get_finding(fid) row>}` (server.py:527). Note the key is `queued`, not `ok`.

**UI** (`FindingCard.doRecheck`, `components/FindingCard.svelte:66-80`): sends `{id}`; the button renders only when `finding.status === "new"` (:220-223), so the 400 for a non-`new` status is reachable only by racing the poll. `!r.ok` -> `console.error` only, no user-visible message; success -> `await store.refresh()`. The `busy` flag is cleared in `finally`, re-arming the button even when the request never reached the server (:77-79).

---

## 4. POST /api/unqueue — remove a `queued` finding from the fix queue

Handler `_unqueue` (server.py:529-548). Mirror of §3:

1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:532-534).
2. no finding -> `404 {"error": "no finding <fid>"}` (server.py:536-539).
3. status != `"queued"` -> `400` `f"finding #{fid} is {finding['status']!r}, not 'queued'"` (server.py:540-544).

**Writes**: `set_status(fid, "new")`; `log_event("unqueue", f"#{fid} removed from fix queue", finding_id=fid)` (server.py:546-547).

**Success**: `200 {"ok": true, "finding": <refreshed row>}` (server.py:548).

**UI** (`FindingCard.doUnqueue`, `components/FindingCard.svelte:82-96`): sends `{id}`; the button renders only when `finding.status === "queued"` (:225-228). `!r.ok` -> `console.error` only; success -> `await store.refresh()`.

---

## 5. POST /api/override — budget override set/clear (+ daemon wake)

Handler `_override` (server.py:550-580).

**Request**: `{"id": int | "all", "mode": "once" | "exempt" | null}` (`mode` absent == null, server.py:553).

**Validation order**:
1. **Special case first**: `id == "all" && mode == null` -> `clear_all_overrides()` -> `UPDATE findings SET budget_override = NULL, updated_at = ? WHERE budget_override IS NOT NULL` + commit, returns `rowcount` (store.py:584-592); `log_event("override", f"cleared all budget overrides ({n} findings)")` (server.py:557, no finding_id); respond `200 {"ok": true, "cleared": n}` (server.py:558). `"all"` with a non-null mode falls through to step 2.
2. non-int id -> `400 {"error": "id must be an integer (or 'all' with mode=null)"}` (server.py:560-562).
3. mode not in `("once", "exempt", None)` -> `400 {"error": "mode must be 'once', 'exempt', or null"}` (server.py:563-565).
4. no finding -> `404 {"error": "no finding <fid>"}` (server.py:567-570).

**Writes**: `set_budget_override(fid, mode)` -> `UPDATE findings SET budget_override = ?, updated_at = ? WHERE id = ?` + commit (store.py:502-510; store re-validates mode, raising ValueError -> 400, unreachable here); `log_event("override", f"#{fid} budget override: {label}", finding_id=fid)` where `label = mode or "cleared"` (server.py:571-577).

**Success**: `200 {"ok": true, "finding": <refreshed row>}` (server.py:578).

**Wake side effect** (server.py:579-580): **after** the response is written, `if mode: _wake.set()` — only when *setting* (`once`/`exempt`), never when clearing. `_wake` is a module-global `threading.Event` (server.py:90). The daemon loop's sleep is a poll loop `while not stop and not _wake: stop.wait(min(remaining, 5.0))` (server.py:1087-1093), so a set event is noticed within ≤5 s and the next cycle attempt starts immediately; `_wake` is cleared at the top of each loop iteration (server.py:1056, 1086). Pinned by `tests/test_override_wake.py` (real HTTP server; asserts wake for once/exempt, and that clear does NOT wake).

**Why the wake cannot be dropped**: without it the override still takes effect — `pick_next`/`backend.decide` read `budget_override` from the DB — but only at the loop's next *natural* wake. Per `_compute_sleep_s` (server.py:934-992) that can be: 60 s (repos enabled, no work), 15 min (idle/unrecognized), 5 min (error), 30 min (denied without retry_at), or **up to 60 min** (denied with retry_at, cap `min(until_retry+30, 60*60)`, floor 60 s). Worst case ≈ 60 minutes — exactly the delay the override exists to bypass. **In Rust this is local, not forwarded**: the scheduler loop runs in this process, so `override_` notifies `AppState.scheduler`'s `wake` directly, and only when *setting* a mode, matching Python's `if mode:` (server.rs:768-774). Under `serve` (no scheduler) the write still lands; there is simply no loop here to wake.

**UI** (`FindingCard.doOverride`, `components/FindingCard.svelte:98-112`): two call shapes, `{id, mode: "once"|"exempt"}` and `{id, mode: null}`, chosen by whether `finding.budget_override` is set (:231-236). `!r.ok` -> `console.error` only; success -> `await store.refresh()`. **Known gap**: the `{"id": "all", "mode": null}` clear-all shape of step 1 has *no* caller in the Svelte UI — the vanilla `app.ts` had a `clearAllOverrides` button and nothing replaced it. The endpoint behaviour is still specified above and still implemented by both daemons; it is simply unreachable from the shipped UI today.

---

## 6. POST /api/repo — update repo fields

Handler `_update_repo` (server.py:582-608).

**Request**: `{"id": int, "enabled"?: any, "url"?: str, "default_branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:585-587).
2. no repo -> `404 {"error": "no repo <rid>"}` (server.py:589-592). `get_repo` (store.py:220-224) — int key -> `WHERE id = ?`.
3. Field extraction (server.py:593-601), silently skipping anything invalid:
   - `enabled` present (any JSON type) -> coerced `1 if truthy else 0`.
   - `url` only if a string that is non-empty after `.strip()` -> stripped value, which must then pass the repo URL rule below. This is the one field in this handler that *rejects* rather than skips: a bad scheme is a `400`, not a dropped field (server.rs:808-818).
   - `default_branch` same rule.
   - `forge` only if literally `"github"` or `"gitlab"`.
4. No field survived -> `400 {"error": "no valid fields to update"}` (server.py:602-604). (So `{"id":1,"forge":"bitbucket"}` yields this, not a forge-specific error.)

**Repo URL rule** — `valid_repo_url` (server.rs:952-975; server.py:727-749). Both daemons enforce the same rule, and the two implementations were compared clause by clause: Python's `url.partition(":")` / `re.fullmatch(r"[A-Za-z][A-Za-z0-9+.\-]*", head)` / `head.lower() in ("http", "https", "ssh")` (server.py:740-749) is the same decision as Rust's `find(':')` / scheme charset check / `["http", "https", "ssh"].iter().any(|s| scheme.eq_ignore_ascii_case(s))` (server.rs:953-974). They must not drift: the two servers share a work root and a database, so a URL one accepts and the other refuses turns a rollback into a behaviour change. The rule, in order:
1. No `:` anywhere -> accepted (server.rs:953-955; server.py:740-742). A bare path is a legal clone source.
2. There is a `:`, but the text before it is not an RFC 3986 scheme — scheme = ALPHA followed by ALPHA/DIGIT/`+`/`-`/`.` — -> accepted (server.rs:956-965; server.py:743-744). This is what keeps scp-like clone URLs working: in `git@github.com:acme/widget.git` the prefix is `git@github.com`, and `@` is not scheme-legal, so the colon is path syntax, not a scheme.
3. There is a scheme -> it must be `http`, `https` or `ssh`, matched case-insensitively (server.rs:972-974; server.py:749). `https://host/x.git`, `HTTPS://host/x.git`, `ssh://git@host/x.git` and `SSH://git@host/x.git` are all accepted; `JavaScript:alert(1)` is not.
4. Any other scheme — `javascript:`, `data:`, `vbscript:` — -> `400 {"error": "url must be http(s) or an ssh clone URL"}` (`ApiError::BadRequest` -> `StatusCode::BAD_REQUEST`, server.rs:100; `self._error(400, ...)` server.py:608, :641).

**Both spellings of an ssh clone URL are accepted**: the scheme-less scp-like `git@host:owner/repo.git` (no scheme, so it passes the first test) and the explicit `ssh://git@host/owner/repo.git` (`ssh` is in the allow-list alongside `http`/`https`, case-insensitively in both daemons). The guard exists to keep a URL that a browser would *execute* out of an `href` — `javascript:`, `data:`, `vbscript:` — not to restrict transports.

This was drafted the other way round, when the allow-list was `http`/`https` only and the explicit `ssh://` form was refused by a message that claimed to permit it. The code was the wrong side of that: the allow-list now includes `ssh`.

The gate is on the write because `repo.url` is rendered as an `<a href>` by the UI, so a stored `javascript:` URL executes on click; validating once at the write is cheaper than escaping every read (server.rs:857-869, server.py:727-733). It guards both write paths in both daemons with the identical message — update (server.rs:811-815; server.py:607-609, before the value reaches `fields`) and add (server.rs:960-963; server.py:640-642, after the non-empty check) — and is pinned on both sides: tests/post_test.rs:812-843 (add: the three executable schemes plus mixed-case `JavaScript:` rejected; `https`, `http`, scp-like `git@github.com:acme/widget.git` and explicit `ssh://git@github.com/acme/widget.git` accepted) and :847-864 (update path); `hunter/tests/test_server.py:546-578` walks the same table against `valid_repo_url`, including `SSH://git@host/x.git` accepted and `ftp://example.com/x.git` rejected — the allow-list is exactly those three schemes, not "anything that is not executable".

**Writes**: `update_repo(rid, **fields)` -> dynamic `UPDATE repos SET <k> = ?, ... WHERE id = ?` + commit; allowed columns `{name, url, default_branch, forge, enabled}` else ValueError (store.py:236-249 — `name` is store-allowed but the handler never sends it). No `updated_at` (column doesn't exist). Then `log_event("repo", f"updated {repo['name']}: {action}")` where `action = ", ".join(f"{k}={v}")` over post-coercion values, e.g. `updated myrepo: enabled=1` (server.py:606-607; no finding_id/job_id).

**Success**: `200 {"ok": true, "repo": <refreshed get_repo(rid) row>}` (server.py:625) — every `repos` column except `deleted_at`, which `_repo_row` strips on the way out (store.py:101-113, 453-458): the 10 schema columns plus the 6 migration-added `last_*_at` ones. Identical shape to a `/api/repos` element (API-CONTRACT.md §7); `/api/summary.repos` is the one that differs.

**UI** (`ReposPage.toggleRepo`, `pages/ReposPage.svelte:94-114`): only the pause/enable toggle, sending `{id, enabled}` where `enabled` is the **integer** `0`/`1`, not a boolean (:95) — step 3's coercion accepts either. Branches on `r.ok`: success -> `toast("<name> enabled|paused", true)` + `store.refresh()`; failure -> `console.error` with the status plus `toast("Failed to toggle <name>", false)`, and the row is left showing its old state (:101-113). `url`/`default_branch`/`forge` updates still have no UI caller.

---

## 7. POST /api/repos — add repo

Handler `_add_repo` (server.py:628-661); Rust `add_repo` (server.rs:989-1092). Cite the function names first — the line numbers below are hints that rot on every edit.

**Request**: `{"name": str, "url": str, "branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. `raw = body.get("name")`; `name = raw.strip() if isinstance(raw, str) else ""` (server.py:632-633). A non-string name is **not** a crash — it becomes `""` and falls into step 2's `400 {"error": "invalid repo name"}`, the same answer Rust gives (§0.2).
2. Empty or failing `re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.\-]*', name)` -> `400 {"error": "invalid repo name"}` (server.py:635-637; Rust `valid_repo_name`, server.rs:980-987, applied at server.rs:1006-1008). First char alnum/underscore; then alnum, `_`, `.`, `-`. No `/`. This is the only name rule there is — see step 8. Pinned by tests/test_server.py:489-516 (bad: `../../etc`, `/etc/passwd`, `..`, `a/../../../b`, `repos/../../x`) and :518-543 (good: `my-repo_1.0`, which also asserts the handler hands the store the repos *directory*, not a name-derived path).
3. `url`: taken only if `isinstance(str)`, stripped; empty -> `400 {"error": "name and url are required"}` (server.py:634, 638-640; server.rs:1009-1017). The stripped value then goes through the **repo URL rule** (§6, `valid_repo_url`) -> `400 {"error": "url must be http(s) or an ssh clone URL"}` on a scheme outside the allow-list (server.py:641-643; server.rs:952-975, applied at server.rs:1018-1022), before `branch`/`forge` are looked at.
4. `branch`: `"main"` unless a non-blank string (server.py:644-645; server.rs:1023-1026).
5. `forge = body.get("forge") or None`; a falsy `forge` (absent, null, `""`) -> `detect_forge(url)` (server.py:646-648; server.rs:1046, the `_` arm of the `forge` match). **Detection is total**: it always returns a forge, so only an *explicit* bad forge reaches step 6's 400.

   The rule is one function per daemon and they agree clause for clause: `detect_forge` is `is_github_host(url_host(url))` -> `github`, else `gitlab` (`detect_forge`, forge.rs:909-915; forge.py:625-649, where the host comes from `_extract_host`, the Python spelling of `url_host`).

   `url_host` / `_extract_host` (forge.rs:870-888; forge.py:601-622) parses exactly the URL forms §6 admits, and nothing else:
   - **`https://`, `http://`, `ssh://`** — the authority is everything up to the first `/` (forge.rs:871-874; forge.py:613-615).
   - **scp-like `git@host:path`** — the authority is everything before the first `:`; that colon is the path separator, not a port (forge.rs:875-877; forge.py:617-620).
   - **Only the scheme is case-folded.** `strip_scheme` compares it with `eq_ignore_ascii_case` (forge.rs:304-311, dispatched by `strip_any_scheme` forge.rs:314-318); Python uses `(?i:https?|ssh)` (forge.py:613). Schemes are case-insensitive per RFC 3986 and §6 stores `HTTPS://…` verbatim, so a case-sensitive compare here left the scheme inside the authority and read `HTTPS` as the host. The host is returned with its case intact and folded only inside `is_github_host`; the path is never folded, because it carries the case-sensitive owner/repo slug (`_url_path`, forge.py:120-130; `github_owner_repo`, forge.rs:270-290). Pinned: `HTTPS://github.com/Acme/Widget.git` -> host `github.com`, forge `github`, slug `Acme/Widget` (`TestCaseInsensitiveScheme`, test_forge.py:414-441).
   - **`user@` is stripped** from the authority, and **`:port` is stripped** — for the scheme forms only, since in the scp form the colon left with the path (forge.rs:879-886; forge.py:621-622). So `ssh://git@gitlab.example.com:2222/g/r.git` has host `gitlab.example.com`.
   - **Anything else has no host**: `None` / `""` (forge.rs:887; forge.py:617-619). There is no hostname fallback. That fallback was the literal `"gitlab.com"`, which filed every `ssh://` URL as GitLab and ran `glab --hostname github.com` against GitHub repos (forge.py:607-611).

   `is_github_host` (forge.rs:258-262; forge.py:133-146) lowercases the host, splits it on `.`, and matches **labels, never substrings**. A host is GitHub iff some label is exactly `github`, **or** its last two labels are `ghe` then `com`. Everything else is GitLab — including the no-host case, since `detect_forge` feeds the empty string straight in (forge.rs:910; forge.py:646-649). There is no `gitlab` matching of any kind: GitLab is purely the remainder. The asymmetry is deliberate (forge.rs:890-908; forge.py:626-645): GitHub is reachable only at domains GitHub operates — `github.com` and Enterprise Cloud's `<org>.ghe.com` — plus Enterprise Server installs, which by convention carry a `github` label; any hostname at all can be a self-hosted GitLab, so GitLab takes the remainder. The label rule is a sanity bound, not an anti-spoofing measure: `github.com.evil.example` has a `github` label and reads as GitHub, and nothing here defends against a hostile URL — the URL is typed by the operator adding their own repo.

   | URL | host | detected | why |
   |---|---|---|---|
   | `https://gitlab.mycompany.com/g/r.git` | `gitlab.mycompany.com` | `gitlab` | no `github` label, not `*.ghe.com`. The `gitlab` label is matched by nothing; it lands in the remainder like any other host |
   | `https://code.mycompany.com/g/r.git` | `code.mycompany.com` | `gitlab` | names neither forge -> remainder. **Not `github`** |
   | `https://mycompany.ghe.com/acme/widget.git` | `mycompany.ghe.com` | `github` | last two labels are `ghe` + `com`; so is `git@mycompany.ghe.com:acme/widget.git` |
   | `https://notgithub.com/acme/widget.git` | `notgithub.com` | `gitlab` | the label is `notgithub`, not `github`. **A substring test would have said GitHub** |
   | `bogus` | (none) | `gitlab` | unparseable -> no host -> remainder |

   Pinned on both sides: `forge_is_detected_from_the_host` (hunter-rs/tests/forge_test.rs:247-302) carries all five rows; `TestDetectForge` (test_forge.py:314-345) and `TestExtractHost` (test_forge.py:290-312) are the Python mirror.
6. `forge not in FORGE_NAMES` (`("github", "gitlab")`, forge.py:594-598) -> `400 {"error": "unknown forge 'bitbucket' (choose from github, gitlab)"}` (repr-quoted, server.py:649-651; Rust parses `ForgeName` and formats the same message, server.rs:1030-1044).
7. Duplicate: `get_repo(name) is not None` -> `409 {"error": "repo 'x' already exists"}` (repr-quoted, server.py:652-655). **Gotcha**: `get_repo` routes all-digit keys to `WHERE id = ?` (store.py:585-590), so a repo *named* `"123"` dupe-checks against repo **id** 123 — a preexisting oddity the regex permits; Rust ports it as-is (server.rs:1051-1061).
8. **No containment check** — there is nothing left to contain. The clone directory is `<work_root>/repos/repo-<id>`, derived from the id SQLite issues, so the handler passes `cfg.work_root / "repos"` to `add_repo` and never joins the name to a path at all (server.py:656-658; server.rs:1062-1073). The step 2 regex is therefore the *entire* name rule, and traversal is structurally impossible rather than filtered: a name of `../../etc` is refused by the regex, and would address no path even if it were not. Earlier revisions resolved `work_root / 'repos' / name` and required the result to be a prefix match under `work_root`; that check went with its premise.

**Writes**: `add_repo(name, url, cfg.work_root / "repos", branch, forge=forge)` (server.py:658) -> one transaction, two statements, because the path needs the id the INSERT has not issued yet: `INSERT INTO repos (name, url, path, forge, default_branch, added_at) VALUES (?,?,'',?,?,?)` with `added_at = now_ms()`, then `UPDATE repos SET path = ? WHERE id = ?` with `repo_dir(repos_dir, rid)` = `<repos_dir>/repo-<id>`; commit; returns `lastrowid` (`Store.add_repo` store.py:552-583, `repo_dir` at store.py:105-113). A row whose `path` never got written would be unusable, hence the single transaction. `enabled` defaults to 1, `last_hunt_*` and `deleted_at` NULL (schema.sql:5-26). Then `log_event("repo", f"added {name} ({forge}) -> {repo_path}")` with `repo_path = repo_dir(cfg.work_root / 'repos', rid)` (server.py:659-660); Rust builds the same message from `Store::repo_dir` (server.rs:1074-1085).

**Success**: `201 {"ok": true, "repo": <get_repo(rid) row>}` (server.py:661; server.rs:1086-1091) — the §6 shape: every `repos` column except `deleted_at`.

**NO git clone happens here.** The clone is deferred to the scheduler:
- `pick_next` selects kind `hunt` for a repo whose `path` doesn't exist ("not cloned yet -> hunt does the clone", scheduler.py:1932-1933).
- `run_hunt`: `rpath.parent.mkdir(parents=True, exist_ok=True)` then `run_cmd(["git", "clone", repo["url"], str(rpath)], timeout=600)` (default cwd); rc != 0 -> `log_event("error", f"hunt {rname}: clone failed: {out[-300:]}")` and cycle summary `{"error": f"clone failed: {out[-300:]}"}` (scheduler.py:157-162). `run_recheck` has the identical clone block with message prefix `recheck #{fid}:` (scheduler.py:386-395). Other kinds refuse with `log_event("error", f"{kind} {rname}: repo not cloned")` / `{"error": "repo not cloned"}` (scheduler.py:581-583, 721-723).
- Path layout: the clone lives at `<work_root>/repos/repo-<id>`; the repo's notes live at `<work_root>/notes/repo-<id>.md`, beside the clones rather than inside one (`repo_dir` store.py:105-113 / store.rs:453-455, `repo_notes_path` store.py:703-727 / `Store::notes_path` store.rs:476-478). Notes are outside the checkout because a file inside it is untracked in a real git repo — a worker doing a broad `git add` commits the operator's private notes into a pull request — and because creating one used to create `repos/repo-<id>/` as a side effect, a directory with no `origin` that `sync_repo` reads as an already-cloned repo and refuses to work in, which made adding a note before a repo's first cycle enough to make it uncloneable for good. Both paths are keyed by id, never by name: names differing only in case are one directory on NTFS and APFS, Windows reserves `CON`/`NUL`/`AUX` and strips trailing dots, and anything past 255 bytes is `ENAMETOOLONG` raised at clone time rather than at add time. The name never reaches a path, so traversal is structurally impossible rather than filtered, and renaming a repo moves no files. Migration 007 rewrote the deployed rows off the historical `repos/<name>` layout; notes still sitting at the old in-clone path are moved once at daemon startup by `migrate_repo_notes` (server.py:786-825; server.rs:1158-1195), which never overwrites — a note already at the new path is the current one.

**UI** (`ui-svelte/src/pages/ReposPage.svelte:136-168`): sends `{name, url, branch}` with `branch` defaulting to `"main"` client-side too, and adds `forge` only when the select is non-empty (`if (newForge)`, :146) — the auto-detect option's value is `""`, so the key is dropped and the server runs step 5. Success is `r.ok`, not a literal `201` (see `api.svelte.ts:20-28` on why comparing to a literal has already shipped as a bug): failure -> `console.error` plus `toast("Failed to add repo", false)` and the dialog stays open; success -> `toast("Added <name>", true)`, the four form fields reset, `store.refresh()`, and the caller closes the dialog (:321).

---

## 8. POST /api/repo/delete — delete repo (guarded)

Handler `_delete_repo` (server.py:662-697; server.rs:1075-1127).

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:665-667; server.rs:1082).
2. Look the repo up: `get_repo(rid)` / `get_repo_by_id(rid)` both filter `deleted_at IS NULL` (store.py:585-590), so this asks "is there an **active** repo?", and a miss splits two ways (server.py:669-682; server.rs:1085-1099):
   - **The row exists but is flagged** (`repo_is_deleted`, store.py:671-680) -> `200 {"ok": true}`, after re-attempting phase two (below). The repo really is deleted — it left every read path the moment it was flagged — so answering "no repo" would be a lie told to a client that is right. This covers both a delete whose reclamation failed and a delete whose response was lost in flight; the retry costs nothing and usefully re-attempts reclamation. That attempt's own failure is ignored exactly as it is on the first pass: the status is `200` either way. Nothing else happens on this path — no second `log_event`, no re-flagging, so repeating the request cannot alter `deleted_at` or the name suffix.
   - **No row at all** -> `404 {"error": "no repo <rid>"}`. Once reclamation succeeds the row is gone and the id is indistinguishable from one that was never issued, so a repeat of a *fully completed* delete 404s, as does an id that was never a repo. The `200` window is exactly the lifetime of the flagged row.
3. `soft_delete_repo(rid)` counts `SELECT COUNT(*) FROM findings WHERE repo_id = ?` and `SELECT COUNT(*) FROM jobs WHERE repo_id = ?`; if either > 0 -> **`400`** with exactly: `repo <rid> has <n> finding(s) and <m> job(s) -- cannot delete without losing history; pause it instead` (store.py:620-669). Python raises `ValueError` and the POST dispatcher renders it as `400 {"error": <message>}` (server.py:451-452); Rust maps `StoreWriteError::Refused` to the same body. Both counts and the flag below share one transaction, opened with `BEGIN IMMEDIATE` because the counts are reads and sqlite3 starts a transaction lazily on the first *write* (store.py:634-643), so a job created concurrently cannot land between the check and the flag.

**Deletion is two-phase, in both daemons.** `repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (API-CONTRACT §7), so a freed id is never handed out again and the phases are not ordering the *id* — they are ordering the *directory*. `<work_root>/repos/repo-<id>` is keyed by the id, removing a multi-gigabyte clone is not instant, and the flagged row is the only record that those files are still there. The row outlives the files because dropping it first would leave a directory nothing owns — and `sync_repo` treats any directory at that path as an existing clone.

- **Phase one (synchronous, inside the request)**: `UPDATE repos SET deleted_at = <now>, name = name || ' (deleted #' || id || ')'` (store.py:660-665). Every repo read path filters `deleted_at IS NULL`, so the repo is gone from the API immediately. The name is released here, not in phase two: `repos.name` is `UNIQUE`, and a flagged row would otherwise keep rejecting the name of a repo the operator has already been told is gone.
- **Phase two (`reap_repo`, server.py:752-783; server.rs:1104-1147)**: remove `<work_root>/repos/repo-<id>` entirely — clone and any worktrees under it — then the notes file `<work_root>/notes/repo-<id>.md`, which lives outside the clone and so survives that removal, and only then `DELETE` the row (`forget_deleted_repo`, itself `WHERE deleted_at IS NOT NULL`). That delete is the moment the repo stops being tracked at all, so it must not run first: a removal that fails or is interrupted has to stay findable. A missing directory and a missing notes file are both success; any other error leaves the row flagged for the next pass. The notes file is removed rather than left behind: nothing else will ever revisit it, and it is the operator's private context for a repo that no longer exists. **`reap_repo` takes the notes lock itself, in both daemons** (server.py:766; server.rs:1118) — a note written between the removals and the row delete recreates a file for a repo with no row left to retry it, and there are two callers to cover: the inline attempt in the delete handler and the `reap_deleted_repos` pass (server.py:828; server.rs:1198-1217), which the handler does not wrap.

Phase two is attempted inline so the common case completes before the response, but **its failure is not the caller's failure**: the response is still `200 {"ok": true}`, a warning naming the path is logged, and the row simply stays flagged until a reaper pass retries — before every cycle in both daemons (server.py:505 and 1176; daemon.rs:305) and, in Rust, additionally at daemon startup (daemon.rs:190). A crash between the phases is safe for the same reason. Until that retry succeeds, step 2's first branch is what the endpoint answers for this id.

**Then**: exactly one `log_event("repo", "deleted <name> (#<rid>)")`, written after phase one (server.py:686; server.rs:1105-1113). One deletion, one event — pinned by `deleting_a_repo_logs_exactly_one_event`. The step 2 retry path logs nothing, so a client retrying a lost response does not double the event either.

**Success**: `200 {"ok": true}` — no embedded object (server.py:697; server.rs:1126). Identical body for a first delete and for a retry against a flagged row; the two are distinguishable only by the log.

**UI** (`ReposPage.removeRepo`, `pages/ReposPage.svelte:116-133`): `confirm('Remove repo "<name>"? This cannot be undone.')` first, then sends `{id}`. Branches on `r.ok`: success -> `toast("Removed <name>", true)` + `store.refresh()`; failure -> `console.error` with the status plus `toast("Failed to remove <name>", false)`. The refusal message of step 3 — the one that names the finding and job counts — is **not** surfaced: the toast is a fixed string, so an operator who hits the guard must read the daemon log or the console to learn why.

---

## 9. POST /api/repo/notes — append a repo note (filesystem write)

Handler `_add_repo_note` (server.py:657-679).

**Request**: `{"id": int, "note": str, "category"?: str|null}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:662-664).
2. `note` not a string or blank after strip -> `400 {"error": "note must be a non-empty string"}` (server.py:665-667).
3. `category` present, non-null, non-string -> `400 {"error": "category must be a string"}` (server.py:668-670).
4. no repo -> `404 {"error": "no repo <rid>"}` (server.py:672-675).
5. `category = category.strip() or None if category else None` (server.py:676) — empty/whitespace category becomes None.

**Filesystem write** `append_repo_note(rid, note.strip(), category)` (store.py:747-771), serialized on the class-level notes lock (store.py:754, `_NOTES_LOCK` store.py:161) against the reclamation that deletes the same file:
- Path: `<work_root>/notes/repo-<id>.md` (`repo_notes_path`, store.py:703-727; Rust `Store::notes_path`, store.rs:476-478); `mkdir(parents=True, exist_ok=True)` on `<work_root>/notes`. Outside the clone, for the reasons §7 gives. `repo_notes_path` re-validates the id and raises `ValueError` for an unknown repo (store.py:723-726) — unreachable from this endpoint, where step 4 has already 404'd it.
- If the file doesn't exist, create with header: `f"# Notes: {repo['name']}\n\nLast updated: {today}\n\n"` with `today = datetime.now(tz=UTC).date()` — renders `YYYY-MM-DD`, **UTC**; the "Last updated" line is written once at creation and never refreshed (store.py:759-764).
- Append (open mode `"a"`): if category, `f"## {category}\n"`; then `f"- [{ts}] {note}\n\n"` with `ts = datetime.now(tz=UTC).strftime("%Y-%m-%d %H:%M")` (**UTC**, minute precision) (store.py:767-771). Separator between entries = the trailing blank line. Category tag is a fresh `## heading` line per note (no dedup across consecutive same-category notes).
- No *file* locking. The notes lock is a process-level mutex — `Store._NOTES_LOCK` (store.py:161), `AppState.repo_notes` (server.rs:65), taken by the Rust handler at server.rs:1309 — so it orders appends against reclamation inside one daemon, which is all that is needed: `<work_root>/hunter.lock` admits one process (Appendix A). Two processes appending would rely on O_APPEND atomicity `[INFERENCE]`.

**Then**: `log_event("repo", f"note added to {repo['name']}" + (f" [{category}]" if category else ""))` (server.py:678).

**Success**: `201 {"ok": true, "notes": <string>}` (server.py:724) where notes is the **bounded re-read** `repo_notes(rid)` (store.py:731-745): full text if ≤ 4000 chars, else `"...(older notes truncated)...\n" + text[-4000:]` (`_MAX_NOTES_CHARS = 4000`, store.py:729).

**UI** (`ReposPage.addNote`, `pages/ReposPage.svelte:199-246`): requires non-blank note text client-side (`toast("Note text is required", false)`, :201-205), sends `{id, note}` plus `category` only when non-empty (:210-214), and guards re-entry with a `notesSaving` set. Branches on `r.ok`, not on `201`: failure -> `console.error` + `toast("Failed to add note", false)`; success -> `toast("Note added", true)`, clear the form, **discard the response body**, `store.repoNotesCache.delete(id)` and re-`GET /api/repo/notes?id=N` via `store.fetchRepoNotes` (`lib/api.svelte.ts:202-213`), with its own failure toasted as `"Note saved, but reloading notes failed"` (:232-242). So the `notes` string in the 201 body has **no consumer in the shipped UI** — an earlier revision of this bullet called it the one success body that is read, which was true of the vanilla `app.ts` and is not true now. The GET side (`/api/repo/notes?id=N`, server.py:328-338) returns the same bounded string, which is what the panel actually renders.

---

## Appendix A. CSRF & concurrency guards

- **CSRF**: the `application/json` prefix gate on all POSTs (server.py:410-413, 415 error) blocks HTML-form/simple cross-origin requests; combined with loopback-only bind `127.0.0.1` (server.py:697) and no CORS headers anywhere. GETs have no gate.
- **Process exclusivity**: exclusive non-blocking `flock` on `<work_root>/hunter.lock`, held for process lifetime; loser exits with a SystemExit message (server.py:58-81). Both `serve()` (server.py:716) and `daemon()` (server.py:1042) take it.
- **Cycle exclusivity**: `_cycle_lock` (server.py:88) — POST /api/cycle (server.py:483), daemon loop (server.py:1055-1083), summary's `cycle_running` probe (server.py:304).
- **Daemon wake**: `_wake` event, set only by /api/override with a non-null mode (server.py:579-580), polled every ≤5 s during daemon sleep (server.py:1087-1093), cleared at loop top (server.py:1056, 1086).
- **No per-row or per-table write locks**: every request builds its own `Store`/connection (server.py:151-154); serialization is SQLite-level (WAL, default 5 s busy timeout). Handlers are not atomic across their multiple store calls (§0.3); e.g. two concurrent verdicts on the same finding interleave at statement granularity — last `UPDATE` wins, both events logged.
- **Response-then-side-effect ordering**: /api/override writes the response *before* `_wake.set()` (server.py:578-580); /api/cycle responds 202 while the cycle thread runs concurrently (server.py:505-506). All other endpoints complete every write before responding.

## Appendix B. Success-body summary table

| Endpoint | Status | Body | Refreshing getter |
|---|---|---|---|
| /api/verdict | 200 | `{"ok": true, "finding": Row}` | `get_finding` (store.py:368-370) |
| /api/cycle | 202 / 409 | `{"started": true}` / `{"error": "busy"}` | — |
| /api/recheck | 200 | `{"queued": true, "finding": Row}` | `get_finding` |
| /api/unqueue | 200 | `{"ok": true, "finding": Row}` | `get_finding` |
| /api/override (all-clear) | 200 | `{"ok": true, "cleared": n}` | — |
| /api/override (per-finding) | 200 | `{"ok": true, "finding": Row}` | `get_finding` |
| /api/repo | 200 | `{"ok": true, "repo": Row}` | `get_repo` (store.py:220-224) |
| /api/repos | 201 | `{"ok": true, "repo": Row}` | `get_repo` |
| /api/repo/delete | 200 | `{"ok": true}` | — |
| /api/repo/notes | 201 | `{"ok": true, "notes": str}` | `repo_notes` (store.py:280-294) |

All finding/repo Rows are `SELECT *` full rows — shapes per round-1 `hunter-rs/API-CONTRACT.md`. **The Svelte UI reads none of them**, including `/api/repo/notes`' `notes` string (§9): every write path calls `store.refresh()` or re-GETs instead. They are soft contract with no consumer today; ship them for parity.
