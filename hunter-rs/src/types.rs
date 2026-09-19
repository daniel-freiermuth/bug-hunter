//! Domain and API response types — THE parity contract with the Python
//! server (see API-CONTRACT.md). JSON key names and presence/null semantics
//! are load-bearing: the vanilla-TS UI consumes these unchanged, and
//! /api/summary is zod-validated client-side (hard contract).
//!
//! Conventions:
//! - SQLite INTEGER -> i64, REAL -> f64, TEXT -> String; nullable -> Option.
//! - `enabled` stays i64 (Python serves 0/1 ints, not booleans).
//! - "key absent" (not null) is modeled as Option + `skip_serializing_if`;
//!   "present but possibly null" as plain Option; "absent OR null" as
//!   Option<Option<T>> (None = absent, Some(None) = null).

use crate::domain::{BugClass, FindingStatus, FindingType, ForgeName, JobKind, JobState, Severity};
use serde::Serialize;

/// Budget gate result: allowed or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetState {
    Allowed,
    Denied,
}

// ---------------------------------------------------------------------------
// Row types (verbatim table columns; explicit column lists in store.rs —
// never SELECT *: the live DB carries residual columns absent from dev.db).
// ---------------------------------------------------------------------------

/// repos row, all 15 columns (10 base + 5 migration-added last_*_at).
/// Served verbatim by /api/repos; /api/summary uses `RepoBrief` instead.
#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub path: String,
    pub forge: ForgeName,
    pub default_branch: String,
    pub last_hunt_sha: Option<String>,
    pub last_hunt_at: Option<i64>,
    pub enabled: i64,
    pub added_at: i64,
    pub last_full_hunt_at: Option<i64>,
    pub last_test_gap_at: Option<i64>,
    pub last_dep_update_at: Option<i64>,
    pub last_refactor_at: Option<i64>,
    pub last_modernization_at: Option<i64>,
    pub last_standards_at: Option<i64>,
}

/// The 10-key repo object inside /api/summary (pydantic strips the 5
/// migration columns there; zod requires exactly these keys).
#[derive(Debug, Clone, Serialize)]
pub struct RepoBrief {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub path: String,
    pub forge: String,
    pub default_branch: String,
    pub last_hunt_sha: Option<String>,
    pub last_hunt_at: Option<i64>,
    pub enabled: i64,
    pub added_at: i64,
}

impl From<&Repo> for RepoBrief {
    fn from(r: &Repo) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            url: r.url.clone(),
            path: r.path.clone(),
            forge: r.forge.to_string(),
            default_branch: r.default_branch.clone(),
            last_hunt_sha: r.last_hunt_sha.clone(),
            last_hunt_at: r.last_hunt_at,
            enabled: r.enabled,
            added_at: r.added_at,
        }
    }
}

/// findings row, all 38 columns (35 schema + 3 migration-added
/// modernization columns). `missing_tests` is JSON-encoded TEXT served
/// as a plain string — do NOT parse it.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: FindingType,
    pub repo_id: i64,
    pub fingerprint: String,
    pub file: Option<String>,
    pub symbol: Option<String>,
    pub line: Option<i64>,
    pub severity: Severity,
    pub confidence: f64,
    pub summary: String,
    pub detail: Option<String>,
    pub status: FindingStatus,
    pub pr_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub bug_class: Option<BugClass>,
    pub evidence_plan: Option<String>,
    pub introduced_by: Option<String>,
    pub rung_achieved: Option<i64>,
    pub verdict_reason: Option<String>,
    pub budget_override: Option<String>,
    pub fix_attempts: i64,
    pub last_fix_failure: Option<String>,
    pub recheck_attempts: i64,
    pub last_recheck_failure: Option<String>,
    pub ecosystem: Option<String>,
    pub package: Option<String>,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub update_type: Option<String>,
    pub security_advisory: Option<String>,
    pub missing_tests: Option<String>,
    pub test_file: Option<String>,
    pub smell_type: Option<String>,
    pub suggested_refactor: Option<String>,
    pub modernization_class: Option<String>,
    pub current_approach: Option<String>,
    pub proposed_approach: Option<String>,
}

impl Finding {
    /// Computed `category` key (API-CONTRACT.md §4): absent for unknown
    /// types (and for known types whose source column is null it is
    /// serialized as null — matching Python, which emits the key with
    /// the column's value).
    pub fn category(&self) -> Option<Option<String>> {
        match self.kind {
            FindingType::Bug => Some(self.bug_class.map(|b| b.to_string())),
            FindingType::DepUpdate => Some(self.update_type.clone()),
            FindingType::TestGap => Some(Some("coverage".to_owned())),
            FindingType::Refactor => Some(self.smell_type.clone()),
            FindingType::Modernization => Some(self.modernization_class.clone()),
            FindingType::Standards => None,
        }
    }
}

/// One row of GET /api/findings: the finding columns flattened, plus the
/// computed/embedded keys.
#[derive(Debug, Clone, Serialize)]
pub struct FindingOut {
    #[serde(flatten)]
    pub finding: Finding,
    /// Absent for unknown finding types; null when the source column is null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<Option<String>>,
    /// Always present, ascending event-id order, [] when empty.
    pub timeline: Vec<Event>,
    /// Present ONLY for status == "`pr_open`" rows that have a `pr_state` row;
    /// value is `pr_state.needs_attention` (string or null).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_attention: Option<Option<String>>,
}

/// jobs row (17 columns) + `repo_name` from the JOIN. The two finding_*
/// keys exist ONLY on /`api/summary.current_job` when the job has a
/// finding — absent (not null) everywhere else.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub kind: JobKind,
    pub repo_id: i64,
    pub finding_id: Option<i64>,
    pub state: JobState,
    pub pid: Option<i64>,
    pub session_file: Option<String>,
    pub cap_tokens: Option<i64>,
    pub tokens_new: Option<i64>,
    pub calls: Option<i64>,
    pub exit_code: Option<i64>,
    pub killed_reason: Option<String>,
    pub notes: Option<String>,
    pub model: Option<String>,
    pub usage_delta: Option<f64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub repo_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding_fingerprint: Option<String>,
}

/// events row.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub message: String,
    pub job_id: Option<i64>,
    pub finding_id: Option<i64>,
}

/// `pr_state` row, all 18 columns the code creates (the live DB may carry
/// residual columns — explicit column list excludes them).
#[derive(Debug, Clone, Serialize)]
pub struct PrState {
    pub finding_id: i64,
    pub pr_number: Option<i64>,
    pub state: Option<String>,
    pub mergeable: Option<String>,
    pub checks: Option<String>,
    pub head_ref: Option<String>,
    pub last_activity_at: Option<i64>,
    pub last_engaged_activity_at: Option<i64>,
    pub needs_attention: Option<String>,
    pub attention_since: Option<i64>,
    pub attention_fingerprint: Option<String>,
    pub addressed_fingerprint: Option<String>,
    pub head_sha: Option<String>,
    pub addressed_head_sha: Option<String>,
    pub synced_at: Option<i64>,
    pub harvested_at: Option<i64>,
    pub harvest_attempts: i64,
    pub last_harvest_failure: Option<String>,
}

/// `scheduler_state` single row (id = 1).
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerState {
    pub id: i64,
    pub state: String,
    pub detail: String,
    pub next_wake_at: Option<i64>,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Composite responses
// ---------------------------------------------------------------------------

/// GET /api/finding?id=N
#[derive(Debug, Serialize)]
pub struct FindingDetail {
    pub jobs: Vec<Job>,
    pub pr_state: Option<PrState>,
}

/// /`api/summary.next_candidate`. Round 1 never constructs one (the Rust
/// serve has no scheduler yet); the shape is fixed for round 2.
#[derive(Debug, Clone, Serialize)]
pub struct NextCandidate {
    pub kind: JobKind,
    pub id: i64,
    pub label: Option<String>,
    pub is_finding: bool,
    pub is_prioritized: bool,
    pub budget_state: BudgetState,
    pub budget_reason: String,
    /// Epoch ms; always null when allowed.
    pub budget_retry_at: Option<f64>,
}

/// /`api/summary.activity_status` — tagged union on "kind". The UI parses
/// this with an exhaustive zod discriminatedUnion: variants carry ONLY
/// the listed fields, and renaming/adding variants is a breaking change.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityStatus {
    Running { job: Box<Job> },
    Working,
    Error { detail: String },
    Paused { candidate: NextCandidate },
    Ready { candidate: NextCandidate },
    Idle,
    WarmingUp,
}

/// GET /api/summary — zod-validated by the UI on every 5s poll.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub backend_status_html: String,
    /// All `FindingStatus` keys, zero-filled.
    pub counts: std::collections::BTreeMap<String, i64>,
    /// Only observed types; may be {}.
    pub type_counts: std::collections::BTreeMap<String, i64>,
    pub repos: Vec<RepoBrief>,
    pub last_cycle: Option<Event>,
    pub cycle_running: bool,
    pub current_job: Option<Job>,
    pub next_candidate: Option<NextCandidate>,
    pub scheduler_state: Option<SchedulerState>,
    pub activity_status: ActivityStatus,
}

// ---------------------------------------------------------------------------
// Stats (GET /api/stats) — SQL aggregate null semantics preserved:
// SUM over zero rows is null, COUNT is 0.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StatsTotals {
    pub jobs: i64,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub total_usage_delta: Option<f64>,
    pub done: Option<i64>,
    pub denied: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct StatsByKind {
    pub kind: JobKind,
    pub jobs: i64,
    pub done: Option<i64>,
    pub failed: Option<i64>,
    pub killed: Option<i64>,
    pub denied: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub avg_tokens: Option<f64>,
    pub total_usage_delta: Option<f64>,
    /// Comma-joined distinct model strings (`GROUP_CONCAT`), e.g. "opus,sonnet".
    pub models: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatsByFinding {
    pub finding_id: i64,
    pub fingerprint: String,
    pub status: String,
    pub severity: String,
    pub jobs: i64,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub total_usage_delta: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub totals: StatsTotals,
    pub by_kind: Vec<StatsByKind>,
    pub by_finding: Vec<StatsByFinding>,
}

// ---------------------------------------------------------------------------
// Worker result (types.py RunResult; consumed by scheduler _record_job)
// ---------------------------------------------------------------------------

/// Outcome of one worker run (harness.py `run_worker`).
#[derive(Debug, Clone)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub killed_reason: Option<String>, // None | "cap" | "wallclock"
    pub tokens_new: i64,
    pub calls: i64,
    pub session_file: Option<String>,
    pub duration_s: f64,
    pub stdout_tail: String,
    pub usage_delta: Option<f64>,
}
