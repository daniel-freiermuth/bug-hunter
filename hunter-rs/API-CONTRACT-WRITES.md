# Hunter write-path API contract (Python -> Rust port, round 2)

Extracted from the Python source at commit state of 2026-09-13. Every claim cites `file:line`.
Sources: `hunter/hunter/server.py` (handlers), `hunter/hunter/store.py` (SQL), `hunter/hunter/types.py` (status enums), `hunter/hunter/forge.py` (forge detection), `hunter/hunter/scheduler.py` (deferred clone), `hunter/schema.sql` (DDL), `hunter/ui/src/app.ts` (consumer), `hunter/tests/` (behavioral pins).

Shared conventions (error envelope `{"error": "<msg>"}`, `_send` headers, Row shapes, per-request `Store`) are in `hunter-rs/API-CONTRACT.md` sections 1-2 and are not repeated here except where write-specific.

---

## 0. Shared POST plumbing

### 0.1 Content-Type gate (CSRF guard) — runs before everything

`do_POST` (server.py:409-449) first reads `Content-Type` (default `''`) and requires `ctype.startswith('application/json')` (server.py:411-412). Failure -> `415 {"error": "Content-Type must be application/json"}` (server.py:413). Notes:

- **Prefix match**: `application/json; charset=utf-8` passes.
- The gate applies to **all 9 routes including `/api/cycle`**, which never reads a body.
- **Known live discrepancy**: the UI's `runCycle` sends `fetch("/api/cycle", { method: "POST" })` with **no** Content-Type header (app.ts:654), and fetch does not add one for a body-less POST `[INFERENCE from fetch spec]` — so the Run Cycle button currently receives **415**, and the button renders "error" (app.ts:655-658 treat only 409 and 202 specially). The Rust port must reproduce the 415 for parity; fixing the UI is a separate change. Every other UI call site sets the header explicitly (app.ts:642, 675, 696, 712, 724, 736, 748, 776, 798, 860).

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
| non-string `name` on `/api/repos`, non-string truthy `reason` on `/api/verdict` | `AttributeError` (`.strip()` on non-str, server.py:614, 455) | `500 {"error": "internal error"}` |

**Python quirk to decide on**: `isinstance(True, int)` is `True`, so JSON `true`/`false` passes every `isinstance(fid, int)` check and is treated as id 1/0. The Rust port should accept only actual integers; no client sends booleans (app.ts always sends numbers).

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

**UI** (app.ts:632-648): sends `{id, status, ...(reason ? {reason} : {})}` — key omitted when empty. Reads only `r.status`; non-200 -> `alert("verdict failed: " + (r.body?.error || r.status))`; always `refresh()` afterwards (never reads the embedded finding). Reason is collected by `prompt()` for rejected/wontfix and required non-empty client-side (app.ts:636-647).

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

**Forwarding-safety verdict for Rust**: `/api/cycle` (and `/api/override`'s wake, §5) are the only two endpoints that touch process-local state (`_cycle_lock`, `_wake`, `backend`). All of that lives in the Python daemon process, and both `serve()` and `daemon()` take an exclusive flock on `<work_root>/hunter.lock` (server.py:58-81, 716, 1042) so a second in-process implementation cannot even start against the same work_root. A Rust server that **proxies POST /api/cycle verbatim to the Python daemon** is therefore safe and exact: 202/409 semantics, lock exclusion vs. the timed loop, reconcile, and backend accounting all stay where they already are. The proxy must forward with `Content-Type: application/json` (or the daemon 415s). Caveat: the Rust server then cannot bind the same port as the daemon — forwarding only makes sense in a topology where Rust serves the UI/reads on its own port and the Python daemon still owns :8377 (or a private port).

**UI** (app.ts:650-663): `api("/api/cycle", { method: "POST" })` — **no Content-Type; currently receives 415**, rendering "error" (see §0.1). Intended handling: 409 -> button text "busy", 202 -> "started…", else "error"; button restored after 2.5 s; `refresh()`.

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

**UI** (app.ts:665-690): sends `{id}`; non-200 -> `alert("recheck failed: " + (r.body?.error || r.status))` and re-enables the button; `refresh()`.

---

## 4. POST /api/unqueue — remove a `queued` finding from the fix queue

Handler `_unqueue` (server.py:529-548). Mirror of §3:

1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:532-534).
2. no finding -> `404 {"error": "no finding <fid>"}` (server.py:536-539).
3. status != `"queued"` -> `400` `f"finding #{fid} is {finding['status']!r}, not 'queued'"` (server.py:540-544).

**Writes**: `set_status(fid, "new")`; `log_event("unqueue", f"#{fid} removed from fix queue", finding_id=fid)` (server.py:546-547).

**Success**: `200 {"ok": true, "finding": <refreshed row>}` (server.py:548).

**UI** (app.ts:691-710): `{id}`; non-200 -> `alert("unqueue failed: ...")`; `refresh()`.

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

**If the wake does not happen** (e.g. a Rust serve process writes `budget_override` to the DB but cannot set the Python daemon's in-process event): the override still takes effect — `pick_next`/`backend.decide` read `budget_override` from the DB — but only at the daemon's next *natural* wake. Per `_compute_sleep_s` (server.py:934-992) that can be up to: 60 s (repos enabled, no work), 15 min (idle/unrecognized), 5 min (error), 30 min (denied without retry_at), or **up to 60 min** (denied with retry_at, cap `min(until_retry+30, 60*60)`, floor 60 s). Worst case ≈ 60 minutes — exactly the delay the override exists to bypass. **Conclusion: a Rust implementation must forward POST /api/override to the Python daemon** (like /api/cycle) or accept up-to-1-h latency on prioritized work.

**UI** (app.ts:709-743): three call shapes — `{id, mode: "once"|"exempt"}` (budgetOverride), `{id, mode: null}` (clearOverride), `{id: "all", mode: null}` (clearAllOverrides). Non-200 -> `alert("override failed: ...")` / `"clear override failed: ..."` / `"clear all overrides failed: ..."`; `refresh()`.

---

## 6. POST /api/repo — update repo fields

Handler `_update_repo` (server.py:582-608).

**Request**: `{"id": int, "enabled"?: any, "url"?: str, "default_branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:585-587).
2. no repo -> `404 {"error": "no repo <rid>"}` (server.py:589-592). `get_repo` (store.py:220-224) — int key -> `WHERE id = ?`.
3. Field extraction (server.py:593-601), silently skipping anything invalid:
   - `enabled` present (any JSON type) -> coerced `1 if truthy else 0`.
   - `url` only if a string that is non-empty after `.strip()` -> stripped value.
   - `default_branch` same rule.
   - `forge` only if literally `"github"` or `"gitlab"`.
4. No field survived -> `400 {"error": "no valid fields to update"}` (server.py:602-604). (So `{"id":1,"forge":"bitbucket"}` yields this, not a forge-specific error.)

**Writes**: `update_repo(rid, **fields)` -> dynamic `UPDATE repos SET <k> = ?, ... WHERE id = ?` + commit; allowed columns `{name, url, default_branch, forge, enabled}` else ValueError (store.py:236-249 — `name` is store-allowed but the handler never sends it). No `updated_at` (column doesn't exist). Then `log_event("repo", f"updated {repo['name']}: {action}")` where `action = ", ".join(f"{k}={v}")` over post-coercion values, e.g. `updated myrepo: enabled=1` (server.py:606-607; no finding_id/job_id).

**Success**: `200 {"ok": true, "repo": <refreshed get_repo(rid) row>}` (server.py:608) — `SELECT * FROM repos`, i.e. **all** columns including the 5 migration-added `last_*_at` ones (round-1 contract: /api/repos vs summary repo shape difference).

**UI** (app.ts:745-755): only `toggleRepo` -> `{id, enabled: boolean}`. Non-200 -> `alert("update repo failed: ...")`; `refresh()`. url/default_branch/forge updates have no UI caller today.

---

## 7. POST /api/repos — add repo

Handler `_add_repo` (server.py:610-640).

**Request**: `{"name": str, "url": str, "branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. `name = body.get('name','').strip()` (server.py:613-614) — non-string name crashes -> 500 (§0.2).
2. Empty or failing `re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.\-]*', name)` -> `400 {"error": "invalid repo name"}` (server.py:616-618). First char alnum/underscore; then alnum, `_`, `.`, `-`. No `/`, so traversal is doubly blocked (regex + step 7). Pinned by tests/test_server.py:422-503 (bad: `../../etc`, `/etc/passwd`, `..`, `a/../../../b`, `repos/../../x`; good: `my-repo_1.0`).
3. `url`: taken only if `isinstance(str)`, stripped; empty -> `400 {"error": "name and url are required"}` (server.py:615, 619-621).
4. `branch`: `"main"` unless a non-blank string (server.py:622-623).
5. `forge = body.get("forge") or None`; if None -> `detect_forge(url)` (server.py:624-626): host via `^https?://([^/]+)` else `^git@([^:]+):` else literal `"gitlab.com"`; lowercased host containing `"github"` -> `github`, containing `"gitlab"` -> `gitlab`, else **`github`** (forge.py:501-519). So detection never fails — only an *explicit* bad forge does.
6. `forge not in FORGE_NAMES` (`("github", "gitlab")`, forge.py:493-498) -> `400 {"error": "unknown forge 'bitbucket' (choose from github, gitlab)"}` (repr-quoted, server.py:627-629).
7. Duplicate: `get_repo(name) is not None` -> `409 {"error": "repo 'x' already exists"}` (repr-quoted, server.py:631-633). **Gotcha**: `get_repo` routes all-digit keys to `WHERE id = ?` (store.py:221), so a repo *named* `"123"` dupe-checks against repo **id** 123 — a preexisting oddity the regex permits; port as-is.
8. `repo_path = (cfg.work_root / 'repos' / name).resolve()`; if `str(repo_path)` doesn't start with `str(cfg.work_root.resolve())` -> `400 {"error": "invalid repo name"}` (server.py:634-637). Defense-in-depth vs. absolute-name replacement via pathlib `/`.

**Writes**: `add_repo(name, url, str(repo_path), branch, forge=forge)` -> `INSERT INTO repos (name, url, path, forge, default_branch, added_at) VALUES (?,?,?,?,?,?)` with `added_at = now_ms()` + commit, returns `lastrowid` (store.py:203-218). `enabled` defaults to 1, `last_hunt_*` NULL (schema.sql:5-16). Then `log_event("repo", f"added {name} ({forge}) -> {repo_path}")` (server.py:639).

**Success**: `201 {"ok": true, "repo": <get_repo(rid) full row>}` (server.py:640).

**NO git clone happens here.** The clone is deferred to the scheduler:
- `pick_next` selects kind `hunt` for a repo whose `path` doesn't exist ("not cloned yet -> hunt does the clone", scheduler.py:1896-1897).
- `run_hunt`: `rpath.parent.mkdir(parents=True, exist_ok=True)` then `run_cmd(["git", "clone", repo["url"], str(rpath)], timeout=600)` (default cwd); rc != 0 -> `log_event("error", f"hunt {rname}: clone failed: {out[-300:]}")` and cycle summary `{"error": f"clone failed: {out[-300:]}"}` (scheduler.py:151-157). `run_recheck` has the identical clone block with message prefix `recheck #{fid}:` (scheduler.py:377-387). Other kinds refuse with `log_event("error", f"{kind} {rname}: repo not cloned")` / `{"error": "repo not cloned"}` (scheduler.py:568-570, 705-707).
- Path layout: clone at `<work_root>/repos/<name>` (server.py:634); NOTES.md at `<work_root>/repos/repo-<id>/NOTES.md` — **id-based, a different directory than the clone** (store.py:269-277).

**UI** (app.ts:766-793): sends `{name, url, branch, forge}` where branch defaults `"main"` client-side too and `forge` is `undefined` when the select is empty (key dropped by JSON.stringify -> server auto-detects). Expects **201**; failure -> `toast("add repo failed: " + (r.body?.error || r.status))`; success clears the dialog, `toast('repo "<name>" added', false)`, `refresh()`.

---

## 8. POST /api/repo/delete — delete repo (guarded)

Handler `_delete_repo` (server.py:642-655).

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:645-647).
2. no repo -> `404 {"error": "no repo <rid>"}` (server.py:649-652).
3. `store.delete_repo(rid)` (store.py:251-266) counts `SELECT COUNT(*) FROM findings WHERE repo_id = ?` and `SELECT COUNT(*) FROM jobs WHERE repo_id = ?`; if either > 0 raises `ValueError` -> **`400`** with exactly: `repo <rid> has <n> finding(s) and <m> job(s) -- cannot delete without losing history; pause it instead` (store.py:255-264; do_POST maps it, server.py:445-446).

**What is deleted**: only `DELETE FROM repos WHERE id = ?` + commit (store.py:265-266). **Nothing on the filesystem** — the clone dir `<work_root>/repos/<name>`, any worktrees, and `<work_root>/repos/repo-<id>/NOTES.md` are all left in place. events rows referencing the repo's findings can't exist (findings count was 0), but repo-kind events mentioning it by name remain.

**Then**: `log_event("repo", f"deleted {repo['name']} (#{rid})")` (server.py:654).

**Success**: `200 {"ok": true}` — no embedded object (server.py:655).

**UI** (app.ts:795-810): `confirm()` first ("Only works if it has no findings or jobs yet."), sends `{id}`; non-200 -> `toast("remove repo failed: ...")`; success -> `toast('repo "<name>" removed', false)`, `refresh()`.

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

**Filesystem write** `append_repo_note(rid, note.strip(), category)` (store.py:296-313):
- Path: `<work_root>/repos/repo-<id>/NOTES.md` (store.py:269-277); `mkdir(parents=True, exist_ok=True)`.
- If the file doesn't exist, create with header: `f"# Notes: {repo['name']}\n\nLast updated: {datetime.now().date()}\n\n"` — date renders `YYYY-MM-DD`, **local time**; the "Last updated" line is written once at creation and never refreshed (store.py:303-306).
- Append (open mode `"a"`): if category, `f"## {category}\n"`; then `f"- [{ts}] {note}\n\n"` with `ts = datetime.now().strftime("%Y-%m-%d %H:%M")` (local time, minute precision) (store.py:308-313). Separator between entries = the trailing blank line. Category tag is a fresh `## heading` line per note (no dedup across consecutive same-category notes).
- No file locking — concurrent appends rely on O_APPEND atomicity `[INFERENCE]`.

**Then**: `log_event("repo", f"note added to {repo['name']}" + (f" [{category}]" if category else ""))` (server.py:678).

**Success**: `201 {"ok": true, "notes": <string>}` (server.py:679) where notes is the **bounded re-read** `repo_notes(rid)` (store.py:280-294): full text if ≤ 4000 chars, else `"...(older notes truncated)...\n" + text[-4000:]` (`_MAX_NOTES_CHARS = 4000`, store.py:278).

**UI** (app.ts:849-871): sends `{id, note, category}` (`category` `undefined`/dropped when empty). Expects **201**; failure -> `toast("add note failed: ...")`; success stores `r.body?.notes || ""` into the notes cache and re-renders (this is the one POST whose success body is actually consumed). The GET side (`/api/repo/notes?id=N`, server.py:328-338) reads the same bounded string.

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

All finding/repo Rows are `SELECT *` full rows — shapes per round-1 `hunter-rs/API-CONTRACT.md`. The UI ignores every embedded object except `/api/repo/notes`' `notes` string (it calls `refresh()` instead); they are soft contract, but ship them for parity.
