// API types — manually maintained to match hunter-rs/src/types.rs.
// The shapes are the wire contract: any change here must match the
// Rust Serialize output. See hunter-rs/API-CONTRACT.md for the spec.

export interface Repo {
  id: number;
  name: string;
  url: string;
  path: string;
  forge: string;
  default_branch: string;
  last_hunt_sha: string | null;
  last_hunt_at: number | null;
  enabled: number; // 0 | 1, not boolean
  added_at: number;
}

export interface Finding {
  type: string;
  id: number;
  repo_id: number;
  fingerprint: string;
  file: string | null;
  symbol: string | null;
  line: number | null;
  severity: string;
  confidence: number;
  summary: string;
  detail: string | null;
  status: string;
  pr_url: string | null;
  created_at: number;
  updated_at: number;
  bug_class?: string | null;
  evidence_plan?: string | null;
  introduced_by?: string | null;
  verdict_reason?: string | null;
  budget_override?: string | null;
  // Type-specific
  ecosystem?: string | null;
  package?: string | null;
  current_version?: string | null;
  latest_version?: string | null;
  update_type?: string | null;
  security_advisory?: string | null;
  missing_tests?: string | null;
  test_file?: string | null;
  smell_type?: string | null;
  suggested_refactor?: string | null;
  modernization_class?: string | null;
  current_approach?: string | null;
  standard_section?: string | null;
  proposed_approach?: string | null;
  // Embedded by /api/findings
  category?: string | null;
  timeline?: Event[];
  needs_attention?: string | null;
}

export interface Job {
  id: number;
  kind: string;
  repo_id: number;
  repo_name: string;
  finding_id: number | null;
  state: string;
  pid?: number | null;
  session_file?: string | null;
  cap_tokens?: number | null;
  tokens_new: number | null;
  calls: number | null;
  exit_code: number | null;
  killed_reason: string | null;
  notes?: string | null;
  model?: string | null;
  usage_delta?: number | null;
  started_at: number | null;
  finished_at: number | null;
  // Only on current_job when finding exists
  finding_summary?: string | null;
  finding_fingerprint?: string | null;
}

export interface Event {
  id: number;
  at: number;
  kind: string;
  message: string;
  job_id: number | null;
  finding_id: number | null;
}

export interface PrState {
  finding_id: number;
  pr_number: number | null;
  state: string | null;
  mergeable: string | null;
  checks: string | null;
  head_ref: string | null;
  last_activity_at: number | null;
  last_engaged_activity_at: number | null;
  needs_attention: string | null;
  attention_since?: number | null;
  synced_at: number | null;
}

export interface SchedulerState {
  state: string;
  detail: string;
  next_wake_at: number | null;
  updated_at: number;
}

export interface NextCandidate {
  kind: string;
  id: number;
  label: string | null;
  is_finding: boolean;
  is_prioritized: boolean;
  budget_state: string;
  budget_reason: string;
  budget_retry_at: number | null;
}

export type ActivityStatus =
  | { kind: "running"; job: Job }
  | { kind: "working" }
  | { kind: "error"; detail: string }
  | { kind: "paused"; candidate: NextCandidate }
  | { kind: "ready"; candidate: NextCandidate }
  | { kind: "idle" }
  | { kind: "warming_up" };

export interface Summary {
  backend_status_html: string;
  counts: Record<string, number>;
  type_counts: Record<string, number>;
  repos: Repo[];
  last_cycle: Event | null;
  cycle_running: boolean;
  current_job: Job | null;
  next_candidate: NextCandidate | null;
  scheduler_state: SchedulerState | null;
  activity_status: ActivityStatus;
}

export interface FindingDetail {
  jobs: Job[];
  pr_state: PrState | null;
}

export interface StatsTotals {
  jobs: number;
  total_tokens: number | null;
  total_calls: number | null;
  total_usage_delta: number | null;
  done: number | null;
  denied: number | null;
}

export interface StatsByKind {
  kind: string;
  jobs: number;
  done: number | null;
  failed: number | null;
  killed: number | null;
  denied: number | null;
  total_tokens: number | null;
  total_calls: number | null;
  avg_tokens: number | null;
  total_usage_delta: number | null;
  models: string | null;
}

export interface StatsByFinding {
  finding_id: number;
  fingerprint: string;
  status: string;
  severity: string;
  jobs: number;
  total_tokens: number | null;
  total_calls: number | null;
  total_usage_delta: number | null;
}

export interface Stats {
  totals: StatsTotals;
  by_kind: StatsByKind[];
  by_finding: StatsByFinding[];
}
