//! Axum router + handlers — the service's HTTP surface.
//! Parity contract: API-CONTRACT.md (JSON shapes in types.rs).
//!
//! Deliberate deviations from the Python implementation it replaces:
//! - Param validation failures return 400 {"error": ...} where Python
//!   leaks a 500 via its `ValueError` catch-all (the UI only reads the
//!   `error` key). EXCEPTION: /api/finding, where missing/non-numeric/
//!   unknown ids all return 404 {"error":"no such finding"} exactly,
//!   matching Python (contract §5).
//! - Static files are served by a hand-rolled fallback, NOT `ServeDir`:
//!   the Python server's extension allow-list, traversal guard, and
//!   404-JSON behavior (contract §2) don't match `ServeDir` semantics.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::domain::{FindingStatus, FindingType, ForgeName, Severity};
use crate::store::{FindingFilter, RepoUpdate, Store, StoreWriteError};
use crate::types::{
    ActivityStatus, BudgetState, Event, Finding, FindingDetail, FindingOut, Job, Repo, RepoBrief,
    Stats, Summary,
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    /// Budget/status brain (round 2+). Summary reads `status_html()` and
    /// `decide()`; POST handlers never touch it directly.
    pub backend: Arc<dyn crate::backend::Backend>,
    /// Wake signal for the daemon scheduler loop (round 3).
    pub wake: Arc<tokio::sync::Notify>,
    /// Base URL of the Python daemon ("<http://127.0.0.1:8377>") — the
    /// TEMPORARY round-2 forward target for POST /api/cycle and
    /// /api/override (in-process _`cycle_lock`/_wake live there). Deleted
    /// in round 3 when the scheduler moves.
    pub py_base: String,
}

/// Response shape for GET /api/repo/notes.
#[derive(Debug, Serialize)]
pub struct RepoNotesResponse {
    pub notes: String,
}

/// Handler-level error. Body is always the `{"error": "<msg>"}` envelope
/// the UI expects on non-200s (contract §1, §13).
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// 409 — duplicate repo name (WRITES contract §7).
    #[error("{0}")]
    Conflict(String),
    /// 415 — POST Content-Type gate (WRITES contract §0.1).
    #[error("Content-Type must be application/json")]
    UnsupportedMediaType,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn error_body(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(msg) => error_body(StatusCode::BAD_REQUEST, &msg),
            Self::NotFound(msg) => error_body(StatusCode::NOT_FOUND, &msg),
            Self::Conflict(msg) => error_body(StatusCode::CONFLICT, &msg),
            Self::UnsupportedMediaType => error_body(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            ),
            Self::Db(err) => {
                tracing::error!(error = %err, "database error");
                error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
            Self::Internal(err) => {
                tracing::error!(error = %err, "internal error");
                error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

/// Reject POST requests that don't originate from localhost.
/// Self-hosted service — no reason to accept remote mutations.
fn require_localhost(headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let host_base = host.split(':').next().unwrap_or("");
    if matches!(host_base, "localhost" | "127.0.0.1" | "::1" | "") {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "POST only accepted from localhost".to_owned(),
        ))
    }
}

/// Refused -> 400 with the store's exact message (WRITES contract §8:
/// "repo <id> has <n> finding(s) and <m> job(s) -- cannot delete without
/// losing history; pause it instead"); Db -> 500 envelope.
impl From<StoreWriteError> for ApiError {
    fn from(err: StoreWriteError) -> Self {
        match err {
            StoreWriteError::Refused(msg) => Self::BadRequest(msg),
            StoreWriteError::Db(err) => Self::Db(err),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/summary", get(summary))
        .route("/api/findings", get(findings))
        .route("/api/finding", get(finding_detail))
        .route("/api/jobs", get(jobs))
        .route("/api/repos", get(repos).post(add_repo))
        .route("/api/repo", post(update_repo))
        .route("/api/repo/notes", get(repo_notes).post(add_repo_note))
        .route("/api/repo/delete", post(delete_repo))
        .route("/api/events", get(events))
        .route("/api/stats", get(stats))
        .route("/api/verdict", post(verdict))
        .route("/api/cycle", post(cycle))
        .route("/api/recheck", post(recheck))
        .route("/api/unqueue", post(unqueue))
        .route("/api/override", post(override_))
        .fallback(static_files)
        // Cache-Control: no-store on EVERY response, static files included
        // (contract §1) — `overriding` so nothing downstream can win.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state)
}

/// Query param lookup with Python's `parse_qs` semantics: blank values are
/// dropped, so empty string == absent (server.py:342-346).
fn param<'q>(params: &'q HashMap<String, String>, key: &str) -> Option<&'q str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

// -- /api/summary (contract §3) ---------------------------------------------

async fn summary(State(state): State<AppState>) -> Result<Json<Summary>, ApiError> {
    use crate::backend::Verdict;
    use crate::scheduler;
    use crate::types::NextCandidate;

    let store = &state.store;
    let counts = store.status_counts().await?;
    let type_counts = store.type_counts().await?;
    let repos: Vec<RepoBrief> = store
        .list_repos()
        .await?
        .iter()
        .map(RepoBrief::from)
        .collect();
    let last_cycle = store.last_cycle_event().await?;
    let current_job = store.current_job().await?;
    let scheduler_state = store.scheduler_state().await?;

    let backend_status_html = state.backend.status_html().await?;
    // Round-3 comment: cycle_running mirrors Python's in-process
    // _cycle_lock, which still lives in the Python daemon in round 2 —
    // this serve never runs cycles, so the honest constant stays false.
    let cycle_running = false;

    // "What's next" preview (server.py:272-301): only when nothing is
    // running, from the SAME pick_next/decide the scheduler itself uses.
    // pick_next errors are swallowed to None (server.py:274-277 try/
    // except); anticipated_tokens/decide errors propagate (500), as in
    // Python where only pick_next sits inside the try.
    let next_candidate: Option<NextCandidate> = if current_job.is_none() {
        match scheduler::pick_next(store, &state.config, None).await {
            Ok(Some(c)) => {
                let anticipated =
                    scheduler::anticipated_tokens(store, &state.config, c.repo_id(), c.job_kind())
                        .await?;
                let outlook = state.backend.decide(anticipated).await?;
                let is_prioritized = c.budget_override().is_some();
                let verdict = if is_prioritized {
                    outlook.prioritized
                } else {
                    outlook.normal
                };
                let (budget_state, budget_reason, budget_retry_at) = match verdict {
                    Verdict::Denied { reason, retry_at } => (BudgetState::Denied, reason, retry_at),
                    Verdict::Granted { reason, .. } => (BudgetState::Allowed, reason, None),
                };
                Some(NextCandidate {
                    kind: c.job_kind(),
                    id: c.target_id(),
                    label: c.label().map(str::to_owned),
                    is_finding: c.job_kind().is_finding(),
                    is_prioritized,
                    budget_state,
                    budget_reason,
                    budget_retry_at,
                })
            }
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(error = %err, "pick_next failed; suppressing candidate preview");
                None
            }
        }
    } else {
        None
    };

    // Full priority chain per contract §3 / server.py:919-931. Working
    // (priority 2, cycle_running) is SKIPPED — cycle_running is hardwired
    // false until the in-process cycle lock moves here in round 3.
    let activity_status = if let Some(job) = &current_job {
        ActivityStatus::Running {
            job: Box::new(job.clone()),
        }
    } else if let Some(s) = scheduler_state.as_ref().filter(|s| s.state == "error") {
        ActivityStatus::Error {
            detail: s.detail.clone(),
        }
    } else if let Some(c) = &next_candidate {
        if c.budget_state == BudgetState::Denied {
            ActivityStatus::Paused {
                candidate: c.clone(),
            }
        } else {
            ActivityStatus::Ready {
                candidate: c.clone(),
            }
        }
    } else if scheduler_state.is_some() {
        ActivityStatus::Idle
    } else {
        ActivityStatus::WarmingUp
    };

    Ok(Json(Summary {
        backend_status_html,
        counts,
        type_counts,
        repos,
        last_cycle,
        cycle_running,
        current_job,
        next_candidate,
        scheduler_state,
        activity_status,
    }))
}

// -- /api/findings (contract §4) --------------------------------------------

async fn findings(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<FindingOut>>, ApiError> {
    let store = &state.store;
    // Default "1"; any other present, non-empty value selects legacy mode:
    // no `type` filter, no `category` key (the UI never sends this param).
    let unified = param(&params, "unified").unwrap_or("1") == "1";

    let mut filter = FindingFilter {
        status: param(&params, "status").and_then(|s| {
            FindingStatus::ALL
                .iter()
                .find(|fs| fs.as_str() == s)
                .copied()
        }),
        ..FindingFilter::default()
    };
    if let Some(repo_key) = param(&params, "repo") {
        let repo = if repo_key.bytes().all(|b| b.is_ascii_digit()) {
            match repo_key.parse::<i64>() {
                Ok(id) => store.get_repo_by_id(id).await?,
                // All digits but past i64::MAX: no such repo id.
                Err(_) => None,
            }
        } else {
            store.get_repo_by_name(repo_key).await?
        };
        let repo = repo.ok_or_else(|| ApiError::BadRequest(format!("unknown repo {repo_key}")))?;
        filter.repo_id = Some(repo.id);
    }
    if let Some(sev) = param(&params, "severity") {
        let sev = Severity::parse(sev)
            .ok_or_else(|| ApiError::BadRequest(format!("invalid severity {sev}")))?;
        filter.min_severity_rank = Some(sev.rank());
    }
    if unified {
        filter.kind = param(&params, "type").and_then(|s| s.parse::<FindingType>().ok());
    }

    let rows = store.list_findings(&filter).await?;
    let mut timelines = store.events_by_finding().await?;
    let attention = store.pr_attention().await?;

    let out = rows
        .into_iter()
        .map(|finding| {
            // `category` only in unified mode; absent for unknown types.
            let category = if unified { finding.category() } else { None };
            let timeline = timelines.remove(&finding.id).unwrap_or_default();
            // Key present ONLY for pr_open rows that HAVE a pr_state row;
            // map hit with a NULL column -> Some(None) -> serialized null.
            let needs_attention = if finding.status == FindingStatus::PrOpen {
                attention.get(&finding.id).cloned()
            } else {
                None
            };
            FindingOut {
                finding,
                category,
                timeline,
                needs_attention,
            }
        })
        .collect();
    Ok(Json(out))
}

// -- /api/finding (contract §5) ----------------------------------------------

async fn finding_detail(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<FindingDetail>, ApiError> {
    let store = &state.store;
    // Missing, non-numeric, and unknown ids are indistinguishable to the
    // client: 404 {"error":"no such finding"} for all three (contract §5).
    let no_such = || ApiError::NotFound("no such finding".to_owned());
    let fid: i64 = param(&params, "id")
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
        .ok_or_else(no_such)?;
    if !store.finding_exists(fid).await? {
        return Err(no_such());
    }
    Ok(Json(FindingDetail {
        jobs: store.jobs_by_finding(fid).await?,
        pr_state: store.get_pr_state(fid).await?,
    }))
}

// -- /api/jobs, /api/repos, /api/events, /api/stats (contract §§6,7,9,10) ----

async fn jobs(State(state): State<AppState>) -> Result<Json<Vec<Job>>, ApiError> {
    Ok(Json(state.store.list_jobs(50).await?))
}

async fn repos(State(state): State<AppState>) -> Result<Json<Vec<Repo>>, ApiError> {
    Ok(Json(state.store.list_repos().await?))
}

async fn events(State(state): State<AppState>) -> Result<Json<Vec<Event>>, ApiError> {
    Ok(Json(state.store.recent_events(100).await?))
}

async fn stats(State(state): State<AppState>) -> Result<Json<Stats>, ApiError> {
    Ok(Json(Stats {
        totals: state.store.stats_totals().await?,
        by_kind: state.store.stats_by_kind().await?,
        by_finding: state.store.stats_by_finding().await?,
    }))
}

// -- /api/repo/notes (contract §8) -------------------------------------------

async fn repo_notes(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<RepoNotesResponse>, ApiError> {
    let raw = param(&params, "id")
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| ApiError::BadRequest("id query param must be an integer".to_owned()))?;
    let repo = match raw.parse::<i64>() {
        Ok(id) => state.store.get_repo_by_id(id).await?,
        // All digits but past i64::MAX: behaves like any nonexistent id.
        Err(_) => None,
    };
    let repo = repo.ok_or_else(|| ApiError::NotFound(format!("no repo {raw}")))?;
    let notes = Store::repo_notes(&state.config.work_root, repo.id);
    Ok(Json(RepoNotesResponse { notes }))
}

// -- static files + 404 fallback (contract §2) --------------------------------

async fn static_files(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return error_body(StatusCode::NOT_FOUND, "not found");
    }
    if path == "/" || path == "/index.html" {
        return match tokio::fs::read(state.config.ui_dir.join("index.html")).await {
            Ok(bytes) => {
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response()
            }
            Err(_) => error_body(StatusCode::NOT_FOUND, "ui/index.html missing"),
        };
    }
    // Python: suffix = path.rsplit(".", 1)[-1] when the path contains a
    // dot; only these three extensions are ever served (contract §2).
    let content_type = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map") => "application/json",
        _ => return error_body(StatusCode::NOT_FOUND, "not found"),
    };
    let asset = state.config.ui_dir.join(path.trim_start_matches('/'));
    // Traversal guard: resolve symlinks and require the target to stay
    // strictly under ui_dir (Python: `UI_DIR in asset.resolve().parents`).
    // Canonicalize failure == missing file == escape attempt == 404 JSON.
    let (Ok(ui_dir), Ok(resolved)) = (
        tokio::fs::canonicalize(&state.config.ui_dir).await,
        tokio::fs::canonicalize(&asset).await,
    ) else {
        return error_body(StatusCode::NOT_FOUND, "not found");
    };
    if !resolved.starts_with(&ui_dir) || resolved == ui_dir {
        return error_body(StatusCode::NOT_FOUND, "not found");
    }
    match tokio::fs::read(&resolved).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type)], bytes).into_response(),
        // Directory or race-deleted file: same 404 as Python's is_file gate.
        Err(_) => error_body(StatusCode::NOT_FOUND, "not found"),
    }
}

// ============================================================================
// POST endpoints (API-CONTRACT-WRITES.md). Shared plumbing first (§0), then
// one handler per contract section. Error strings are byte-copied from the
// contract; deliberate deviations are commented ACCEPTED DEVIATION inline.
// ============================================================================

/// Shared reqwest client for the TEMPORARY round-2 forwarding shim
/// (/api/cycle relay + /api/override wake — the Python daemon owns
/// _`cycle_lock`/_wake). `AppState` is frozen, so the lazily-built client
/// lives in a static instead of a field. Deleted in round 3.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
    &CLIENT
}

/// Content-Type gate (contract §0.1): prefix match on "application/json"
/// (so "; charset=utf-8" passes), applied to every POST — including
/// /api/cycle, which never reads a body. Missing header == "".
fn post_gate(headers: &HeaderMap) -> Result<(), ApiError> {
    // Localhost-only: self-hosted service, no remote mutations.
    require_localhost(headers)?;
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ctype.starts_with("application/json") {
        Ok(())
    } else {
        Err(ApiError::UnsupportedMediaType)
    }
}

/// Body parse (contract §0.2). "empty body" is byte-exact Python; malformed
/// JSON gets a stable message instead of Python's json.JSONDecodeError text
/// (serde messages differ anyway); valid-JSON-non-object returns a 400
/// envelope where Python leaks a raw `TypeError` as `500 internal error`
/// (ACCEPTED DEVIATION — keep OUR error envelope, the UI only displays it).
fn parse_object(body: &[u8]) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest("empty body".to_owned()));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::BadRequest("invalid JSON body".to_owned()))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ApiError::BadRequest(
            "body must be a JSON object".to_owned(),
        )),
    }
}

/// `isinstance(x, int)` with the boolean quirk closed: JSON true/false are
/// NOT integers here (contract §0.2 decision — Python treats them as 1/0),
/// and floats are rejected exactly like Python.
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn require_int(body: &Map<String, Value>, key: &str, msg: &str) -> Result<i64, ApiError> {
    body.get(key)
        .and_then(as_int)
        .ok_or_else(|| ApiError::BadRequest(msg.to_owned()))
}

/// Python truthiness over a JSON value (for /api/repo's `enabled` coercion
/// and the falsy-forge/-reason fallthroughs).
fn python_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

async fn fetch_finding(store: &Store, fid: i64) -> Result<Finding, ApiError> {
    store
        .get_finding(fid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no finding {fid}")))
}

async fn fetch_repo(store: &Store, rid: i64) -> Result<Repo, ApiError> {
    store
        .get_repo_by_id(rid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no repo {rid}")))
}

// -- POST /api/verdict (contract §1) ------------------------------------------

async fn verdict(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    // Non-string status is simply "not a verdict status", like Python.
    let status_str = body.get("status").and_then(Value::as_str).unwrap_or("");
    let status = FindingStatus::ALL
        .iter()
        .find(|fs| fs.as_str() == status_str)
        .copied()
        .filter(|fs| fs.is_verdict());
    let Some(status) = status else {
        // ACCEPTED DEVIATION: Python's f-string renders StrEnum reprs here
        // (`<Status.QUEUED: 'queued'>`, ...). We emit the plain value list —
        // the UI only ever displays this string.
        return Err(ApiError::BadRequest(
            "status must be one of ['queued', 'rejected', 'wontfix', 'note', 'merged']".to_owned(),
        ));
    };
    // (body.get("reason") or "").strip() or None — absent/null/empty/
    // whitespace (and Python-falsy non-strings) all become None
    // (server.py:455). ACCEPTED DEVIATION: a truthy non-string reason 400s
    // here where Python leaks a 500 AttributeError.
    let reason = match body.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        }
        Some(other) if !python_truthy(other) => None,
        Some(_) => {
            return Err(ApiError::BadRequest("reason must be a string".to_owned()));
        }
    };
    if status.reason_required() && reason.is_none() {
        return Err(ApiError::BadRequest(format!(
            "reason required for status '{status}'"
        )));
    }
    let finding = fetch_finding(&state.store, fid).await?;
    match reason.as_deref() {
        Some(r) => {
            state.store.set_finding_verdict(fid, status, r).await?;
        }
        None => {
            state.store.set_finding_status(fid, status).await?;
        }
    }
    // fingerprint from the row read BEFORE the update (server.py:473-479).
    let mut msg = format!("finding {fid} [{}] -> {status}", finding.fingerprint);
    if let Some(reason) = &reason {
        msg.push_str(": ");
        msg.push_str(reason);
    }
    state
        .store
        .log_event("verdict", &msg, None, Some(fid))
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/cycle (contract §2) --------------------------------------------

/// TEMPORARY round-2 forwarding shim: /api/cycle mutates Python-process
/// state (_`cycle_lock`, backend singleton, scheduler), so we relay the POST
/// verbatim and pipe back status+body. Our §0.1 gate runs FIRST, so the
/// UI's missing-Content-Type bug still 415s here exactly like Python; the
/// forward adds the header (the daemon would 415 otherwise) since our gate
/// already passed. Deleted in round 3 when the scheduler moves.
async fn cycle(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let url = format!("{}/api/cycle", state.py_base);
    let forwarded = http_client()
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    match forwarded {
        Ok(resp) => {
            // Convert via u16 — reqwest's http types are not nominally ours.
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            Ok((
                status,
                [(header::CONTENT_TYPE, "application/json")],
                axum::body::Body::from(body),
            )
                .into_response())
        }
        Err(err) => {
            tracing::warn!(error = %err, "cycle forward failed (round-2 shim)");
            Ok(error_body(
                StatusCode::BAD_GATEWAY,
                "python daemon unreachable (round-2 forwarding)",
            ))
        }
    }
}

// -- POST /api/recheck, /api/unqueue (contract §§3,4) --------------------------

async fn recheck(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    let finding = fetch_finding(&state.store, fid).await?;
    if finding.status != FindingStatus::New {
        return Err(ApiError::BadRequest(format!(
            "finding #{fid} is '{}', not 'new'",
            finding.status
        )));
    }
    state
        .store
        .set_finding_status(fid, FindingStatus::Rechecking)
        .await?;
    state
        .store
        .log_event(
            "recheck",
            &format!("#{fid} queued for recheck"),
            None,
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // NB: success key is `queued`, not `ok` (contract §3).
    Ok(Json(json!({ "queued": true, "finding": refreshed })).into_response())
}

async fn unqueue(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    let finding = fetch_finding(&state.store, fid).await?;
    if finding.status != FindingStatus::Queued {
        return Err(ApiError::BadRequest(format!(
            "finding #{fid} is '{}', not 'queued'",
            finding.status
        )));
    }
    state
        .store
        .set_finding_status(fid, FindingStatus::New)
        .await?;
    state
        .store
        .log_event(
            "unqueue",
            &format!("#{fid} removed from fix queue"),
            None,
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/override (contract §5) ------------------------------------------

async fn override_(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let mode_val = body.get("mode").cloned().unwrap_or(Value::Null);
    // Special case FIRST (server.py:554-558): {"id": "all", "mode": null}
    // clears every override; "all" with a non-null mode falls through to
    // the int check below.
    if body.get("id").and_then(Value::as_str) == Some("all") && mode_val.is_null() {
        let n = state.store.clear_all_overrides().await?;
        state
            .store
            .log_event(
                "override",
                &format!("cleared all budget overrides ({n} findings)"),
                None,
                None,
            )
            .await?;
        return Ok(Json(json!({ "ok": true, "cleared": n })).into_response());
    }
    let fid = require_int(
        &body,
        "id",
        "id must be an integer (or 'all' with mode=null)",
    )?;
    let mode = match &mode_val {
        Value::Null => None,
        Value::String(s) if s == "once" || s == "exempt" => Some(s.clone()),
        _ => {
            return Err(ApiError::BadRequest(
                "mode must be 'once', 'exempt', or null".to_owned(),
            ));
        }
    };
    fetch_finding(&state.store, fid).await?;
    state
        .store
        .set_budget_override(fid, mode.as_deref())
        .await?;
    let label = mode.as_deref().unwrap_or("cleared");
    state
        .store
        .log_event(
            "override",
            &format!("#{fid} budget override: {label}"),
            None,
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // TEMPORARY round-2 double-write (deleted in round 3): when SETTING an
    // override, the Python handler must also run so its in-process `_wake`
    // event fires (contract §5 — otherwise the daemon only notices at its
    // next natural wake, up to ~60 min). The Python handler re-writes the
    // same DB value, which is idempotent. Python fires the wake after
    // responding; we spawn before returning (closest axum equivalent) and
    // log-and-ignore failures — the forward is never fatal.
    if let Some(mode) = mode {
        // Wake the daemon scheduler loop so it picks up the override promptly.
        state.wake.notify_one();
        // TEMPORARY round-2 double-write (deleted in round 3): forward to
        // the Python daemon so its in-process `_wake` fires too.
        let url = format!("{}/api/override", state.py_base);
        let payload = json!({ "id": fid, "mode": mode }).to_string();
        tokio::spawn(async move {
            let result = http_client()
                .post(&url)
                .header(header::CONTENT_TYPE, "application/json")
                .body(payload)
                .timeout(Duration::from_secs(2))
                .send()
                .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, "override wake forward failed (round-2 shim)");
            }
        });
    }
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/repo (contract §6) ----------------------------------------------

async fn update_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let repo = fetch_repo(&state.store, rid).await?;
    // Silent-skip field coercion (server.py:593-601): invalid values are
    // dropped, never rejected. `action` mirrors Python's dict insertion
    // order (enabled, url, default_branch, forge).
    let mut fields = RepoUpdate::default();
    let mut action: Vec<String> = Vec::new();
    if let Some(v) = body.get("enabled") {
        let enabled = i64::from(python_truthy(v));
        action.push(format!("enabled={enabled}"));
        fields.enabled = Some(enabled);
    }
    if let Some(Value::String(s)) = body.get("url") {
        let s = s.trim();
        if !s.is_empty() {
            action.push(format!("url={s}"));
            fields.url = Some(s.to_owned());
        }
    }
    if let Some(Value::String(s)) = body.get("default_branch") {
        let s = s.trim();
        if !s.is_empty() {
            action.push(format!("default_branch={s}"));
            fields.default_branch = Some(s.to_owned());
        }
    }
    if let Some(Value::String(s)) = body.get("forge")
        && let Ok(f) = s.trim().parse::<ForgeName>()
    {
        action.push(format!("forge={f}"));
        fields.forge = Some(f);
    }
    if action.is_empty() {
        // Exact message from server.py:602-604 (so {"id":1,"forge":
        // "bitbucket"} yields this, not a forge-specific error).
        return Err(ApiError::BadRequest("no valid fields to update".to_owned()));
    }
    state.store.update_repo(rid, &fields).await?;
    state
        .store
        .log_event(
            "repo",
            &format!("updated {}: {}", repo.name, action.join(", ")),
            None,
            None,
        )
        .await?;
    let refreshed = fetch_repo(&state.store, rid).await?;
    Ok(Json(json!({ "ok": true, "repo": refreshed })).into_response())
}

// -- POST /api/repos (contract §7) -----------------------------------------------

/// re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.\-]*', name) without a regex
/// dependency (server.py:616-618): ASCII alnum/underscore first, then
/// alnum/underscore/dot/hyphen. No '/', so traversal is blocked here too.
fn valid_repo_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Port of `detect_forge` (forge.py:501-519). Host from `^https?://([^/]+)`,
/// else `^git@([^:]+):`, else the literal fallback "gitlab.com" — yes, an
/// unparseable url detects as gitlab. Detection never fails; only an
/// explicit bad forge does.
fn detect_forge(url: &str) -> ForgeName {
    let host = ["https://", "http://"]
        .iter()
        .find_map(|p| url.strip_prefix(p))
        .and_then(|rest| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            url.strip_prefix("git@")
                .and_then(|rest| rest.split_once(':'))
                .map(|(host, _)| host)
                .filter(|h| !h.is_empty())
        })
        .unwrap_or("gitlab.com");
    let host = host.to_ascii_lowercase();
    if host.contains("github") {
        ForgeName::Github
    } else if host.contains("gitlab") {
        ForgeName::Gitlab
    } else {
        ForgeName::Github
    }
}

async fn add_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    // ACCEPTED DEVIATION: a non-string name 500s in Python (.strip()
    // AttributeError, contract §0.2); here it falls into "invalid repo
    // name" like any other unusable name.
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if name.is_empty() || !valid_repo_name(&name) {
        return Err(ApiError::BadRequest("invalid repo name".to_owned()));
    }
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err(ApiError::BadRequest("name and url are required".to_owned()));
    }
    let branch = match body.get("branch").and_then(Value::as_str).map(str::trim) {
        Some(b) if !b.is_empty() => b.to_owned(),
        _ => "main".to_owned(),
    };
    let forge: ForgeName = match body.get("forge") {
        // Python-falsy forge (`body.get("forge") or None`) -> auto-detect.
        Some(v) if python_truthy(v) => match v.as_str() {
            Some(f) => match f.parse::<ForgeName>() {
                Ok(fg) => fg,
                Err(_) => {
                    return Err(ApiError::BadRequest(format!(
                        "unknown forge '{f}' (choose from github, gitlab)"
                    )));
                }
            },
            // Truthy non-string: JSON-render it in the message (Python
            // repr-renders; no client sends this shape).
            None => {
                return Err(ApiError::BadRequest(format!(
                    "unknown forge '{v}' (choose from github, gitlab)"
                )));
            }
        },
        _ => detect_forge(&url),
    };
    // Dupe check ports Python's get_repo(name) key routing: an all-digit
    // name looks up BY ID (store.py:221), so a repo named "123" dupe-checks
    // against repo id 123 — preexisting oddity, ported as-is (contract §7).
    let existing = if name.bytes().all(|b| b.is_ascii_digit()) {
        match name.parse::<i64>() {
            Ok(id) => state.store.get_repo_by_id(id).await?,
            Err(_) => None,
        }
    } else {
        state.store.get_repo_by_name(&name).await?
    };
    if existing.is_some() {
        return Err(ApiError::Conflict(format!("repo '{name}' already exists")));
    }
    // (work_root / "repos" / name).resolve() containment (server.py:634-637).
    // Rust's canonicalize() needs an existing path, so the check is lexical —
    // equivalent here because the name regex already forbids separators and
    // leading dots, making escape impossible.
    let repo_path = state.config.work_root.join("repos").join(&name);
    if !repo_path.starts_with(&state.config.work_root) {
        return Err(ApiError::BadRequest("invalid repo name".to_owned()));
    }
    let path_str = repo_path.to_string_lossy().into_owned();
    // NO git clone here — deferred to the scheduler: pick_next selects a
    // hunt for a repo whose path doesn't exist, and run_hunt does the clone
    // (contract §7, scheduler.py:151-157).
    let rid = state
        .store
        .add_repo(&name, &url, &path_str, &branch, forge)
        .await?;
    state
        .store
        .log_event(
            "repo",
            &format!("added {name} ({forge}) -> {path_str}"),
            None,
            None,
        )
        .await?;
    let repo = fetch_repo(&state.store, rid).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "repo": repo })),
    )
        .into_response())
}

// -- POST /api/repo/delete (contract §8) ----------------------------------------

async fn delete_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let repo = fetch_repo(&state.store, rid).await?;
    // StoreWriteError::Refused -> 400 with the store's exact message via
    // From<StoreWriteError>. Nothing on the filesystem is touched.
    state.store.delete_repo(rid).await?;
    // Clean up notes file — SQLite can reuse INTEGER PRIMARY KEY ids,
    // so a new repo could inherit stale notes.
    let notes_path = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"))
        .join("NOTES.md");
    let _ = std::fs::remove_file(&notes_path);
    state
        .store
        .log_event(
            "repo",
            &format!("deleted {} (#{rid})", repo.name),
            None,
            None,
        )
        .await?;
    Ok(Json(json!({ "ok": true })).into_response())
}

// -- POST /api/repo/notes (contract §9) -------------------------------------------

async fn add_repo_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let note = match body.get("note") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => {
            return Err(ApiError::BadRequest(
                "note must be a non-empty string".to_owned(),
            ));
        }
    };
    // Present, non-null, non-string -> 400; empty/whitespace string -> None.
    let category = match body.get("category") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        }
        Some(_) => {
            return Err(ApiError::BadRequest("category must be a string".to_owned()));
        }
    };
    let repo = fetch_repo(&state.store, rid).await?;
    let notes = Store::append_repo_note(
        &state.config.work_root,
        rid,
        &repo.name,
        &note,
        category.as_deref(),
    )
    .map_err(|err| ApiError::Internal(err.into()))?;
    let mut msg = format!("note added to {}", repo.name);
    if let Some(category) = &category {
        use std::fmt::Write;
        let _ = write!(msg, " [{category}]");
    }
    state.store.log_event("repo", &msg, None, None).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "notes": notes })),
    )
        .into_response())
}
