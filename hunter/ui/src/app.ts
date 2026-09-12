// Idle-Token Bug Hunter — triage dashboard

import { z } from "zod";

// ---------------------------------------------------------------------------
// API types (mirror hunter.types / hunter.store)
// ---------------------------------------------------------------------------

// Zod schemas double as the runtime-validated wire contract AND (via
// z.infer) the compile-time type -- one definition, not two that can
// silently drift. Only the /api/summary boundary is validated this way
// (see refresh() below); Finding/Job/Event/Repo below it stay plain
// interfaces for the OTHER endpoints (findings/jobs/events/repos) that
// aren't in scope here, though Job/Event/Repo/CurrentJob/NextCandidate/
// SchedulerState/ActivityStatus/Summary further down ARE zod-derived,
// since /api/summary embeds all of them.

interface Finding {
  type: string;  // 'bug' | 'dep_update' | 'test_gap' | 'refactor' | 'modernization'
  id: number;
  repo_id: number;
  fingerprint: string;
  file: string;
  symbol: string | null;
  line: number | null;
  category: string;  // bug_class | update_type | 'coverage' | smell_type | modernization_class
  bug_class?: string;  // legacy field for bugs
  severity: string;
  confidence: number;
  summary: string;
  detail: string | null;
  evidence_plan: string | null;
  introduced_by: string | null;
  // Type-specific fields
  ecosystem?: string | null;
  package?: string | null;
  current_version?: string | null;
  latest_version?: string | null;
  update_type?: string | null;
  security_advisory?: string | null;
  missing_tests?: string | null;
  smell_type?: string | null;
  suggested_refactor?: string | null;
  modernization_class?: string | null;
  current_approach?: string | null;
  proposed_approach?: string | null;
  // Common status fields
  status: string;
  verdict_reason: string | null;
  pr_url: string | null;
  created_at: number;
  updated_at: number;
  timeline: Event[];
  budget_override: string | null;
  needs_attention: string | null;
}

const JobSchema = z.object({
  id: z.number(),
  kind: z.string(),
  repo_id: z.number(),
  repo_name: z.string(),
  finding_id: z.number().nullable(),
  state: z.string(),
  tokens_new: z.number().nullable(),
  calls: z.number().nullable(),
  exit_code: z.number().nullable(),
  killed_reason: z.string().nullable(),
  started_at: z.number().nullable(),
  finished_at: z.number().nullable(),
});
type Job = z.infer<typeof JobSchema>;

const EventSchema = z.object({
  id: z.number(),
  at: z.number(),
  kind: z.string(),
  message: z.string(),
  job_id: z.number().nullable(),
  finding_id: z.number().nullable(),
});
type Event = z.infer<typeof EventSchema>;

// pr_state table row -- see schema.sql. Only ever fetched on demand via
// /api/finding (toggleFindingDetail below), never part of the 5s poll.
interface PrState {
  pr_number: number | null;
  state: string | null;
  mergeable: string | null;
  checks: string | null;
  head_ref: string | null;
  last_activity_at: number | null;
  last_engaged_activity_at: number | null;
  needs_attention: string | null;
  synced_at: number | null;
}

// /api/finding?id=<id> response: everything about one finding NOT
// already on its list-view card -- see hunter.server._finding_detail's
// docstring for why (list_jobs()'s /api/jobs feed is capped at 50 and
// list_findings() only ever embeds needs_attention for pr_open).
interface FindingDetail {
  jobs: Job[];
  pr_state: PrState | null;
}

const RepoSchema = z.object({
  id: z.number(),
  name: z.string(),
  url: z.string(),
  path: z.string(),
  forge: z.string(),
  default_branch: z.string(),
  last_hunt_sha: z.string().nullable(),
  last_hunt_at: z.number().nullable(),
  enabled: z.number(),
  added_at: z.number(),
});
type Repo = z.infer<typeof RepoSchema>;

const CurrentJobSchema = JobSchema.extend({
  finding_summary: z.string().nullable().optional(),
  finding_fingerprint: z.string().nullable().optional(),
});
type CurrentJob = z.infer<typeof CurrentJobSchema>;

const NextCandidateSchema = z.object({
  kind: z.string(),
  id: z.number(),
  label: z.string().nullable(),
  is_finding: z.boolean(),
  budget_state: z.string(), // "allowed" | "denied" | "exempt"
  budget_reason: z.string(),
  budget_retry_at: z.number().nullable(),
});
type NextCandidate = z.infer<typeof NextCandidateSchema>;

const SchedulerStateSchema = z.object({
  state: z.string(), // "idle" | "denied" | "error"
  detail: z.string(),
  next_wake_at: z.number().nullable(),
  updated_at: z.number(),
});
type SchedulerState = z.infer<typeof SchedulerStateSchema>;

// The single, canonical answer to "what is hunter doing right now" --
// computed once server-side (see hunter.server._activity_status, whose
// docstring names the four incidents this replaced) so this panel and
// the manual-run button can never independently disagree about it
// again. A real discriminated union, mirroring the Python TypedDict
// union exactly (one variant per kind, only the fields that kind
// actually has -- no "job: null" on a variant that was never running):
// renderActivity's switch below is checked exhaustively against this
// by assertNever, so adding a kind here without a case there is a
// compile error, not a silent gap. z.discriminatedUnion also means a
// malformed or unrecognized "kind" value from the wire is rejected at
// parse time, before renderActivity ever sees it.
const ActivityStatusSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("running"), job: CurrentJobSchema }),
  z.object({ kind: z.literal("working") }),
  z.object({ kind: z.literal("error"), detail: z.string() }),
  z.object({ kind: z.literal("paused"), candidate: NextCandidateSchema }),
  z.object({ kind: z.literal("ready"), candidate: NextCandidateSchema }),
  z.object({ kind: z.literal("idle") }),
  z.object({ kind: z.literal("warming_up") }),
]);
type ActivityStatus = z.infer<typeof ActivityStatusSchema>;

const SummarySchema = z.object({
  backend_status_html: z.string(),
  counts: z.record(z.string(), z.number()),
  type_counts: z.record(z.string(), z.number()),
  repos: z.array(RepoSchema),
  last_cycle: EventSchema.nullable(),
  cycle_running: z.boolean(),
  current_job: CurrentJobSchema.nullable(),
  next_candidate: NextCandidateSchema.nullable(),
  scheduler_state: SchedulerStateSchema.nullable(),
  activity_status: ActivityStatusSchema,
});
type Summary = z.infer<typeof SummarySchema>;

interface ApiResult<T> {
  status: number;
  body: T | null;
}

interface KindStats {
  kind: string;
  jobs: number;
  done: number;
  failed: number;
  killed: number;
  denied: number;
  total_tokens: number | null;
  total_calls: number | null;
  avg_tokens: number | null;
  total_usage_delta: number | null;
  models: string | null;
}

interface FindingStats {
  finding_id: number;
  fingerprint: string;
  status: string;
  severity: string;
  jobs: number;
  total_tokens: number | null;
  total_calls: number | null;
  total_usage_delta: number | null;
}

interface Stats {
  totals: {
    jobs: number;
    total_tokens: number | null;
    total_calls: number | null;
    total_usage_delta: number | null;
    done: number;
    denied: number;
  };
  by_kind: KindStats[];
  by_finding: FindingStats[];
}

// ---------------------------------------------------------------------------
// Severity ranking
// ---------------------------------------------------------------------------

const SEV_RANK: Record<string, number> = { high: 3, medium: 2, low: 1 };

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el;
}

function $select(id: string): HTMLSelectElement {
  return $(id) as HTMLSelectElement;
}

function $input(id: string): HTMLInputElement {
  return $(id) as HTMLInputElement;
}

function $button(id: string): HTMLButtonElement {
  return $(id) as HTMLButtonElement;
}

const ESC_MAP: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

function esc(s: unknown): string {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ESC_MAP[c] ?? c);
}

// Exhaustiveness check for discriminated unions (the TS analog of
// Rust's "match must cover every enum variant, or it's a compile
// error") -- calling this with a value TypeScript hasn't already
// narrowed to `never` is itself a compile error, so a switch that
// forgets a case fails to build instead of silently falling through
// at runtime.
function assertNever(x: never): never {
  throw new Error(`unreachable: unhandled variant ${JSON.stringify(x)}`);
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

function ktok(n: number | null): string {
  if (n == null) return "\u2013";
  if (n < 1000) return String(n);
  return Math.round(n / 1000) + "k";
}

function ts(ms: number | null): string {
  if (!ms) return "\u2013";
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function datetime(ms: number | null): string {
  if (!ms) return "\u2013";
  const d = new Date(ms);
  return d.toLocaleDateString([], { month: "short", day: "numeric" }) +
    " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function countdown(ms: number | null): string {
  if (!ms) return "";
  let d = Math.round((ms - Date.now()) / 1000);
  const sign = d < 0 ? "-" : "";
  d = Math.abs(d);
  const h = Math.floor(d / 3600);
  const m = Math.floor((d % 3600) / 60);
  return sign + (h ? h + "h" + String(m).padStart(2, "0") + "m" : m + "m");
}

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return Math.round(n / 1_000) + "k";
  return String(Math.round(n));
}

function dur(j: Job): string {
  if (!j.started_at) return "\u2013";
  const end = j.finished_at || Date.now();
  return Math.round((end - j.started_at) / 1000) + "s";
}

// ---------------------------------------------------------------------------
// Filter checkbox management
// ---------------------------------------------------------------------------

// Module-level state: which values are checked per filter group.
// Key = DOM id (e.g. "fRepo", "pfType"). Value = set of checked values.
// Empty set = "all" (nothing excluded). Survives DOM rebuilds.
const filterState = new Map<string, Set<string>>();

function getFilterSet(id: string): Set<string> {
  let s = filterState.get(id);
  if (!s) { s = new Set(); filterState.set(id, s); }
  return s;
}

/** Is the filter in "all" mode? (empty set = nothing excluded = all) */
function isFilterAll(id: string): boolean {
  const s = filterState.get(id);
  return !s || s.size === 0;
}

/** Get the set of checked values, or null if "all". */
function getChecked(id: string): Set<string> | null {
  return isFilterAll(id) ? null : filterState.get(id)!;
}

/** Build a checkbox group's inner HTML. Static options (like type) pass
 *  fixedOptions; dynamic ones (repo, class) pass nothing and populate later. */
function checkboxGroupHtml(
  id: string,
  label: string,
  options: { value: string; label: string }[],
): string {
  const sel = getFilterSet(id);
  const allChecked = sel.size === 0;
  const items = options.map((o) => {
    const checked = allChecked || sel.has(o.value) ? "checked" : "";
    return `<label class="cb-item"><input type="checkbox" value="${esc(o.value)}" ${checked}>${esc(o.label)}</label>`;
  }).join("");
  return `<div class="filter-group" data-filter-id="${esc(id)}">
    <span class="fg-label">${esc(label)}</span>
    <div class="fg-body">
      <label class="cb-item cb-all"><input type="checkbox" value="__all__" ${allChecked ? "checked" : ""}>All</label>
      ${items}
    </div>
  </div>`;
}

/** Populate a dynamic checkbox group (repo, class) with new values,
 *  preserving checked state. */
function populateCheckboxGroup(id: string, values: string[]): void {
  const container = document.querySelector(`[data-filter-id="${id}"] .fg-body`);
  if (!container) return;
  const sel = getFilterSet(id);
  const allChecked = sel.size === 0;
  // Keep "All" checkbox, rebuild the rest
  const allCb = container.querySelector('.cb-all');
  const existing = new Map<string, HTMLLabelElement>();
  for (const lbl of container.querySelectorAll<HTMLLabelElement>('.cb-item:not(.cb-all)')) {
    const v = lbl.querySelector('input')?.value;
    if (v) existing.set(v, lbl);
  }
  const wanted = new Set(values);
  // Remove stale
  for (const [v, lbl] of existing) {
    if (!wanted.has(v)) { lbl.remove(); sel.delete(v); }
  }
  // Add new
  for (const v of values) {
    if (!existing.has(v)) {
      const lbl = document.createElement('label');
      lbl.className = 'cb-item';
      const checked = allChecked || sel.has(v);
      lbl.innerHTML = `<input type="checkbox" value="${esc(v)}" ${checked ? "checked" : ""}>${esc(v)}`;
      container.appendChild(lbl);
    }
  }
}

/** Wire checkbox change events for a filter group. */
function wireCheckboxGroup(id: string, onChange: () => void): void {
  const container = document.querySelector(`[data-filter-id="${id}"] .fg-body`);
  if (!container) return;
  container.addEventListener("change", (e) => {
    const input = e.target as HTMLInputElement;
    if (!input?.matches('input[type="checkbox"]')) return;
    const sel = getFilterSet(id);
    if (input.value === "__all__") {
      // "All" toggled: if checked, clear the set (= all selected);
      // if unchecked, do nothing (can't have nothing selected)
      if (input.checked) {
        sel.clear();
        for (const cb of container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')) {
          cb.checked = true;
        }
      }
    } else {
      if (input.checked) {
        sel.add(input.value);
      } else {
        sel.delete(input.value);
      }
      // If all individual boxes are now checked, switch to "all" mode
      const allCbs = container.querySelectorAll<HTMLInputElement>('input:not([value="__all__"])');
      const allBox = container.querySelector<HTMLInputElement>('input[value="__all__"]');
      const allIndividualChecked = [...allCbs].every(cb => cb.checked);
      if (allIndividualChecked) {
        sel.clear();
        if (allBox) allBox.checked = true;
      } else {
        // If we're in "all" mode (empty set) but one was unchecked,
        // switch to explicit mode: add all EXCEPT the unchecked one
        if (sel.size === 0) {
          for (const cb of allCbs) {
            if (cb.checked) sel.add(cb.value);
          }
        }
        if (allBox) allBox.checked = false;
      }
    }
    onChange();
  });
}

// Type options are static (known at build time)
const TYPE_OPTIONS = [
  { value: "bug", label: "\ud83d\udc1b Bug" },
  { value: "dep_update", label: "\ud83d\udce6 Dep" },
  { value: "test_gap", label: "\ud83e\uddea Test" },
  { value: "refactor", label: "\u267b\ufe0f Refactor" },
  { value: "modernization", label: "\ud83d\udd2c Modern" },
];
const SEV_OPTIONS = [
  { value: "high", label: "high" },
  { value: "medium", label: "medium" },
  { value: "low", label: "low" },
];
const STATUS_OPTIONS = [
  "new", "rechecking", "queued", "fixing", "pr_open",
  "merged", "rejected", "wontfix", "note",
];

function filterBarHtml(p: string, includeStatus: boolean): string {
  const statusHtml = includeStatus
    ? checkboxGroupHtml(`${p}Status`, "status", STATUS_OPTIONS.map(s => ({ value: s, label: s }))) +
      '<div class="sep"></div>'
    : "";
  return `${statusHtml}
    ${checkboxGroupHtml(`${p}Repo`, "repo", [])}
    <div class="sep"></div>
    ${checkboxGroupHtml(`${p}Type`, "type", TYPE_OPTIONS)}
    ${checkboxGroupHtml(`${p}Class`, "class", [])}
    <div class="sep"></div>
    ${checkboxGroupHtml(`${p}Sev`, "min severity", SEV_OPTIONS)}
    <div class="sep"></div>
    <label>min confidence</label>
    <input type="range" id="${p}Conf" min="0" max="100" value="0" step="5">
    <span id="${p}ConfVal">0%</span>
    <div class="sep"></div>
    <label>sort</label><select id="${p}Sort">
      <option value="score">severity \u00d7 confidence</option>
      <option value="newest">newest first</option>
      <option value="oldest">oldest first</option>
      <option value="repo">by repo</option>
    </select>`;
}
function applyFindingFilters(findings: Finding[], p: string): Finding[] {
  const fStatus = getChecked(`${p}Status`);
  const fRepo = getChecked(`${p}Repo`);
  const fType = getChecked(`${p}Type`);
  const fClass = getChecked(`${p}Class`);
  const fSev = getChecked(`${p}Sev`);
  const fConf = parseInt($input(`${p}Conf`).value, 10) / 100;
  const fSort = $select(`${p}Sort`).value;

  const filtered = findings.filter((f) => {
    if (fStatus && !fStatus.has(f.status)) return false;
    if (fRepo) {
      const repo = f.fingerprint.split(":")[0];
      if (!fRepo.has(repo)) return false;
    }
    if (fType && !fType.has(f.type)) return false;
    if (fClass) {
      const cls = f.category || f.bug_class || "";
      if (!fClass.has(cls)) return false;
    }
    if (fSev) {
      // "min severity" with checkboxes: include if finding's severity is in the checked set
      if (!fSev.has(f.severity)) return false;
    }
    if ((f.confidence || 0) < fConf) return false;
    return true;
  });

  if (fSort === "score") {
    filtered.sort(
      (a, b) =>
        (SEV_RANK[b.severity] || 0) * (b.confidence || 0) -
        (SEV_RANK[a.severity] || 0) * (a.confidence || 0),
    );
  } else if (fSort === "newest") {
    filtered.sort((a, b) => (b.created_at || 0) - (a.created_at || 0));
  } else if (fSort === "oldest") {
    filtered.sort((a, b) => (a.created_at || 0) - (b.created_at || 0));
  } else if (fSort === "repo") {
    filtered.sort((a, b) => a.fingerprint.localeCompare(b.fingerprint));
  }
  return filtered;
}

// ---------------------------------------------------------------------------
// API client
// ---------------------------------------------------------------------------

async function api<T>(
  path: string,
  opts?: RequestInit,
): Promise<ApiResult<T>> {
  const r = await fetch(path, opts);
  let body: T | null = null;
  try {
    body = (await r.json()) as T;
  } catch {
    /* empty */
  }
  return { status: r.status, body };
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function verdict(
  id: number,
  status: string,
  needReason: boolean,
): Promise<void> {
  let reason: string | null = null;
  if (needReason) {
    reason = prompt(`Reason for ${status} (required):`);
    if (reason === null) return;
    reason = reason.trim();
    if (!reason) {
      alert("A non-empty reason is required.");
      return;
    }
  }
  const r = await api<{ error?: string }>("/api/verdict", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, status, ...(reason ? { reason } : {}) }),
  });
  if (r.status !== 200) {
    alert("verdict failed: " + (r.body?.error || r.status));
  }
  refresh();
}

async function runCycle(): Promise<void> {
  const btn = $button("runCycle");
  btn.disabled = true;
  const r = await api<unknown>("/api/cycle", { method: "POST" });
  if (r.status === 409) {
    btn.textContent = "busy";
  } else {
    btn.textContent = r.status === 202 ? "started\u2026" : "error";
  }
  setTimeout(() => {
    btn.textContent = "Run Cycle";
    btn.disabled = false;
  }, 2500);
  refresh();
}

async function recheck(id: number): Promise<void> {
  const btn = document.getElementById("rc" + id) as HTMLButtonElement | null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "Queuing\u2026";
  }
  const r = await api<{ error?: string }>("/api/recheck", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id }),
  });
  if (r.status !== 200) {
    alert("recheck failed: " + (r.body?.error || r.status));
    if (btn) {
      btn.disabled = false;
      btn.textContent = "Recheck";
    }
  }
  refresh();
}

async function unqueue(id: number): Promise<void> {
  const btn = document.getElementById("uq" + id) as HTMLButtonElement | null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "Removing\u2026";
  }
  const r = await api<{ error?: string }>("/api/unqueue", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id }),
  });
  if (r.status !== 200) {
    alert("unqueue failed: " + (r.body?.error || r.status));
    if (btn) {
      btn.disabled = false;
      btn.textContent = "Unqueue";
    }
  }
  refresh();
}

async function budgetOverride(id: number, mode: "once" | "exempt"): Promise<void> {
  const r = await api<{ error?: string }>("/api/override", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, mode }),
  });
  if (r.status !== 200) {
    alert("override failed: " + (r.body?.error || r.status));
  }
  refresh();
}

async function clearOverride(id: number): Promise<void> {
  const r = await api<{ error?: string }>("/api/override", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, mode: null }),
  });
  if (r.status !== 200) {
    alert("clear override failed: " + (r.body?.error || r.status));
  }
  refresh();
}

async function clearAllOverrides(): Promise<void> {
  const r = await api<{ error?: string }>("/api/override", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id: "all", mode: null }),
  });
  if (r.status !== 200) {
    alert("clear all overrides failed: " + (r.body?.error || r.status));
  }
  refresh();
}

async function toggleRepo(id: number, enabled: boolean): Promise<void> {
  const r = await api<{ error?: string }>("/api/repo", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, enabled }),
  });
  if (r.status !== 200) {
    alert("update repo failed: " + (r.body?.error || r.status));
  }
  refresh();
}

function toast(message: string, isError = true): void {
  const el = document.createElement("div");
  el.className = "msg" + (isError ? " err" : "");
  el.textContent = message;
  $("toast").appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

async function addRepo(): Promise<void> {
  const name = $input("arName").value.trim();
  const url = $input("arUrl").value.trim();
  const branch = $input("arBranch").value.trim() || "main";
  const forge = $select("arForge").value || undefined;
  if (!name || !url) {
    toast("name and url are required");
    return;
  }
  const r = await api<{ error?: string }>("/api/repos", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, url, branch, forge }),
  });
  if (r.status !== 201) {
    toast("add repo failed: " + (r.body?.error || r.status));
    return;
  }
  $input("arName").value = "";
  $input("arUrl").value = "";
  $input("arBranch").value = "main";
  $select("arForge").value = "";
  ($("addRepoDialog") as HTMLDialogElement).close();
  toast(`repo "${name}" added`, false);
  refresh();
}

async function removeRepo(id: number, name: string): Promise<void> {
  if (!confirm(`Remove repo "${name}"? Only works if it has no findings or jobs yet.`)) {
    return;
  }
  const r = await api<{ error?: string }>("/api/repo/delete", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id }),
  });
  if (r.status !== 200) {
    toast("remove repo failed: " + (r.body?.error || r.status));
    return;
  }
  toast(`repo "${name}" removed`, false);
  refresh();
}

// ---------------------------------------------------------------------------
// Inline repo notes -- a <details> per row. Fetched content and in-progress
// draft text live in module-level maps (not the DOM) so the 5s refresh()
// rebuild of #repos never loses them; see the ontoggle/restore wiring below.
// ---------------------------------------------------------------------------

const repoNotesCache = new Map<number, string>();
const repoNotesDraft = new Map<number, { category: string; note: string }>();

async function toggleRepoNotes(id: number, opened: boolean): Promise<void> {
  if (!opened) return;
  if (!repoNotesCache.has(id)) {
    const r = await api<{ notes: string; error?: string }>(`/api/repo/notes?id=${id}`);
    if (r.status !== 200) {
      toast("load notes failed: " + (r.body?.error || r.status));
      return;
    }
    repoNotesCache.set(id, r.body?.notes || "");
  }
  renderRepoNotesBody(id);
}

function renderRepoNotesBody(id: number): void {
  const body = document.getElementById(`rn-body-${id}`);
  if (!body) return;
  const content = repoNotesCache.get(id) || "";
  const draft = repoNotesDraft.get(id);
  body.innerHTML = `
    <pre>${esc(content) || "(no notes yet)"}</pre>
    <div class="rn-form">
      <input type="text" class="rn-category" placeholder="category (optional)" value="${esc(draft?.category || "")}">
      <textarea class="rn-note" rows="2" placeholder="note text">${esc(draft?.note || "")}</textarea>
      <button onclick="addRepoNote(${id})">Add Note</button>
    </div>`;
  const catInput = body.querySelector<HTMLInputElement>(".rn-category")!;
  const noteInput = body.querySelector<HTMLTextAreaElement>(".rn-note")!;
  const saveDraft = () => repoNotesDraft.set(id, { category: catInput.value, note: noteInput.value });
  catInput.oninput = saveDraft;
  noteInput.oninput = saveDraft;
}

async function addRepoNote(id: number): Promise<void> {
  const body = document.getElementById(`rn-body-${id}`);
  const note = body?.querySelector<HTMLTextAreaElement>(".rn-note")?.value.trim() || "";
  const category = body?.querySelector<HTMLInputElement>(".rn-category")?.value.trim() || undefined;
  if (!note) {
    toast("note text is required");
    return;
  }
  const r = await api<{ notes: string; error?: string }>("/api/repo/notes", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, note, category }),
  });
  if (r.status !== 201) {
    toast("add note failed: " + (r.body?.error || r.status));
    return;
  }
  repoNotesCache.set(id, r.body?.notes || "");
  repoNotesDraft.delete(id);
  renderRepoNotesBody(id);
  toast("note added", false);
}

// ---------------------------------------------------------------------------
// Finding detail -- a <details> per card (findingCard below), lazily
// fetching /api/finding on first open. Same caching pattern as repo
// notes above: fetched content lives in a module-level map, not the
// DOM, so the 5s refresh() rebuild of #inbox/#allFindings never loses
// it or re-fetches it.
// ---------------------------------------------------------------------------

const findingDetailCache = new Map<number, FindingDetail | "loading">();

async function toggleFindingDetail(id: number, opened: boolean): Promise<void> {
  if (!opened) return;
  if (!findingDetailCache.has(id)) {
    findingDetailCache.set(id, "loading");
    renderFindingDetailBody(id);
    const r = await api<FindingDetail>(`/api/finding?id=${id}`);
    if (r.status === 200 && r.body) {
      findingDetailCache.set(id, r.body);
    } else {
      // Don't pin the failure in the cache -- a transient error would
      // otherwise permanently block retrying (has() stays true forever).
      // Show the failure for this one render pass via the `failed` flag,
      // then leave the cache empty so the next toggle re-fetches.
      findingDetailCache.delete(id);
      renderFindingDetailBody(id, true);
      return;
    }
  }
  renderFindingDetailBody(id);
}

function renderFindingDetailBody(id: number, failed = false): void {
  const el = document.getElementById(`fd-body-${id}`);
  if (!el) return;
  const state = findingDetailCache.get(id);
  if (state === undefined) {
    el.innerHTML = failed
      ? '<div class="empty">failed to load</div>'
      : '<div class="empty">loading\u2026</div>';
    return;
  }
  if (state === "loading") {
    el.innerHTML = '<div class="empty">loading\u2026</div>';
    return;
  }
  const { jobs, pr_state } = state;
  const jobsHtml = jobs.length
    ? `<table>
        <tr><th>id</th><th>kind</th><th>state</th><th class="num">tokens</th>
            <th class="num">calls</th><th class="num">dur</th><th>killed</th><th>when</th></tr>
        ${jobs
          .map(
            (j) => `<tr>
          <td>${j.id}</td><td>${esc(j.kind)}</td>
          <td class="state-${esc(j.state)}">${esc(j.state)}</td>
          <td class="num">${ktok(j.tokens_new)}</td>
          <td class="num">${j.calls ?? "\u2013"}</td>
          <td class="num">${dur(j)}</td>
          <td>${esc(j.killed_reason || "")}</td>
          <td>${datetime(j.started_at)}</td>
        </tr>`,
          )
          .join("")}
      </table>`
    : '<div class="empty">no jobs recorded for this finding</div>';
  const prHtml = pr_state
    ? `<div class="loc">PR #${pr_state.pr_number ?? "?"} \u00b7 ${esc(pr_state.state || "?")}` +
      `${pr_state.mergeable ? " \u00b7 " + esc(pr_state.mergeable) : ""}` +
      `${pr_state.checks ? " \u00b7 " + esc(pr_state.checks) : ""}` +
      `${pr_state.head_ref ? " \u00b7 " + esc(pr_state.head_ref) : ""}</div>
       <div class="loc">last activity ${datetime(pr_state.last_activity_at)} \u00b7 ` +
      `we engaged ${datetime(pr_state.last_engaged_activity_at)} \u00b7 ` +
      `synced ${datetime(pr_state.synced_at)}` +
      `${pr_state.needs_attention ? " \u00b7 \u26a0 " + esc(pr_state.needs_attention) : ""}</div>`
    : '<div class="empty">no PR opened yet</div>';
  el.innerHTML = `<div class="fd-section"><b>Jobs (${jobs.length})</b>${jobsHtml}</div>
    <div class="fd-section"><b>PR state</b>${prHtml}</div>`;
}

// Expose to onclick handlers in rendered HTML
Object.assign(window, {
  verdict,
  recheck,
  unqueue,
  budgetOverride,
  clearOverride,
  clearAllOverrides,
  toggleRepo,
  addRepo,
  removeRepo,
  toggleRepoNotes,
  addRepoNote,
  toggleFindingDetail,
});

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

function renderWindows(html: string): void {
  const winEl = $("windows");
  if (winEl) winEl.innerHTML = html;
}

function renderActivity(s: Summary): void {
  const a: ActivityStatus = s.activity_status;
  const ss: SchedulerState | null = s.scheduler_state;
  const rows: string[] = [];

  // Every branch below renders a.kind, computed once server-side by
  // hunter.server._activity_status and covered by its own test suite --
  // this function does formatting only, never re-derives "what's
  // happening" from current_job/next_candidate/cycle_running itself.
  // See _activity_status's docstring for why that split matters.
  switch (a.kind) {
    case "running": {
      const cj: CurrentJob = a.job;
      const label =
        cj.finding_id != null
          ? `#${cj.finding_id} ${esc(cj.finding_summary || cj.finding_fingerprint || "")}`
          : esc(cj.repo_name);
      rows.push(
        `<div class="row"><b class="running">\u25b6 running</b> ${esc(cj.kind)}: ${label}` +
          ` <span class="dim">(${dur(cj)}, job #${cj.id})</span></div>`,
      );
      break;
    }
    case "working":
      rows.push(
        '<div class="row"><b class="running">\u25b6 running</b> cycle in progress\u2026</div>',
      );
      break;
    case "error":
      rows.push(`<div class="row"><b class="error">\u26a0 error</b> ${esc(a.detail)}</div>`);
      break;
    case "paused": {
      const nc: NextCandidate = a.candidate;
      const label = nc.is_finding ? `#${nc.id} ${esc(nc.label || "")}` : esc(nc.label || "");
      rows.push(
        `<div class="row"><b class="paused">\u23f8 paused</b> next up: ${esc(nc.kind)} ${label}` +
          ` \u00b7 budget: <span class="b-denied">denied</span> (${esc(nc.budget_reason)})</div>`,
      );
      if (nc.budget_retry_at) {
        rows.push(
          `<div class="row dim">budget available ~${countdown(nc.budget_retry_at)} (${ts(nc.budget_retry_at)})</div>`,
        );
      }
      if (ss?.next_wake_at) {
        // "next check" only means "the daemon loop wakes up again" --
        // with sync_prs running nearly every cycle to never delay
        // noticing PR feedback, that wake is often just a cheap
        // heartbeat, not a real chance to start a job while the budget
        // gate is still shut.
        const heartbeatOnly = nc.budget_retry_at != null && ss.next_wake_at < nc.budget_retry_at;
        const label2 = heartbeatOnly ? "next sync check" : "next check";
        const note = heartbeatOnly ? " \u2014 budget still closed" : "";
        rows.push(
          `<div class="row dim">${label2} ~${countdown(ss.next_wake_at)} (${ts(ss.next_wake_at)})${note}</div>`,
        );
      }
      break;
    }
    case "ready": {
      const nc: NextCandidate = a.candidate;
      const label = nc.is_finding ? `#${nc.id} ${esc(nc.label || "")}` : esc(nc.label || "");
      rows.push(
        `<div class="row"><b class="ready">\u25b7 ready</b> next up: ${esc(nc.kind)} ${label}` +
          ` \u00b7 budget: <span class="b-${esc(nc.budget_state)}">${esc(nc.budget_state)}</span></div>`,
      );
      if (ss?.next_wake_at) {
        // No heartbeat-vs-real-check ambiguity here -- budget already
        // allows it, so the next wake IS the start.
        rows.push(
          `<div class="row dim">starting ~${countdown(ss.next_wake_at)} (${ts(ss.next_wake_at)})</div>`,
        );
      }
      break;
    }
    case "idle":
      rows.push('<div class="row"><b class="idle">\u25cf idle</b> nothing to do</div>');
      if (ss?.next_wake_at) {
        rows.push(
          `<div class="row dim">next check ~${countdown(ss.next_wake_at)} (${ts(ss.next_wake_at)})</div>`,
        );
      }
      break;
    case "warming_up":
      rows.push('<div class="row dim">warming up \u2014 no cycle has run yet</div>');
      break;
    default:
      assertNever(a);
  }

  // Last log -- what the most recently COMPLETED cycle actually did.
  // Occasionally interesting, but it's history, not current state --
  // kept visually secondary (small, faint) and last, below everything
  // that describes right now.
  if (ss) {
    rows.push(`<div class="row last-log">${esc(ss.detail)}</div>`);
  }

  $("activity").innerHTML = rows.join("");
}

function findingCard(f: Finding, withActions: boolean): string {
  const sev = esc(f.severity || "?");
  const conf =
    f.confidence != null
      ? Math.round(f.confidence * 100) + "%"
      : "?";
  const loc = `${esc(f.file || "")}${f.line ? ":" + f.line : ""}${f.symbol ? " \u00b7 " + esc(f.symbol) : ""}`;
  const detail = (f.detail || "").trim();
  const plan = (f.evidence_plan || "").trim();
  const tl = f.timeline || [];
  const detailBlock = `<details ontoggle="toggleFindingDetail(${f.id}, this.open)">
    <summary>full history (jobs \u00b7 PR)</summary>
    <div id="fd-body-${f.id}"></div>
  </details>`;
  const timeline = tl.length
    ? `<details><summary>timeline (${tl.length})</summary>
      <div class="timeline">${tl
        .map(
          (e) =>
            `<div class="tl-entry"><span class="t">${datetime(e.at)}</span> <span class="k">${esc(e.kind)}</span> ${esc(e.message)}</div>`,
        )
        .join("")}</div>
    </details>`
    : "";
  const typeLabels: Record<string, string> = {
    bug: "🐛 Bug",
    dep_update: "📦 Dep",
    test_gap: "🧪 Test",
    refactor: "♻️ Refactor",
    modernization: "🔬 Modern",
  };
  const typeLabel = typeLabels[f.type] || esc(f.type) || "?";
  const category = f.category || f.bug_class || "";
  const approach =
    f.current_approach || f.proposed_approach
      ? `<div class="loc">${esc(f.current_approach || "?")} \u2192 ${esc(f.proposed_approach || "?")}</div>`
      : "";

  return `<div class="card">
    <div class="top">
      <span class="badge type-${esc(f.type) || "bug"}">${esc(typeLabel)}</span>
      <span class="badge sev-${sev}">${sev} \u00b7 ${conf}</span>
      <span class="badge">${esc(category)}</span>
      <span class="badge status-${esc(f.status)}">${esc(f.status)}</span>
      <span class="fp">#${f.id} ${esc(f.fingerprint)}</span>
    </div>
    <div class="sum">${esc(f.summary)}</div>
    <div class="loc">${loc}${f.introduced_by ? " \u00b7 introduced by " + esc(f.introduced_by) : ""}</div>
    ${approach}
    ${
      detail || plan
        ? `<details><summary>detail + evidence plan</summary>
      ${detail ? `<pre>${esc(detail)}</pre>` : ""}
      ${plan ? `<pre>evidence plan:\n${esc(plan)}</pre>` : ""}
    </details>`
        : ""
    }
    ${timeline}
    ${detailBlock}
    ${
      withActions
        ? `<div class="acts">
      <button class="q" onclick="verdict(${f.id},'queued',false)">Queue fix</button>
      <button class="rc" id="rc${f.id}" onclick="recheck(${f.id})">Recheck</button>
      <button class="r" onclick="verdict(${f.id},'rejected',true)">Reject</button>
      <button class="r" onclick="verdict(${f.id},'wontfix',true)">Wontfix</button>
      <button onclick="verdict(${f.id},'note',false)">Note</button>
    </div>`
        : ""
    }
  </div>`;
}

function renderPipeline(findings: Finding[]): void {
  const cols: [string, (f: Finding) => boolean][] = [
    ["rechecking", (f) => f.status === "rechecking"],
    ["queued", (f) => f.status === "queued"],
    ["fixing", (f) => f.status === "fixing"],
    ["pr_review", (f) => f.status === "pr_open" && !!f.needs_attention],
    ["pr_open", (f) => f.status === "pr_open" && !f.needs_attention],
    ["merged", (f) => f.status === "merged"],
  ];
  $("pipeline").innerHTML = cols
    .map(([name, pred]) => {
      const items = findings.filter(pred);
      const body = items.length
        ? items
            .map(
              (f) => {
                const ov = f.budget_override;
                const ovBadge = ov ? ` <span class="ov">${esc(ov)}</span>` : "";
                const acts: string[] = [];
                if (name === "queued") {
                  acts.push(`<button class="uq" id="uq${f.id}" onclick="unqueue(${f.id})">Unqueue</button>`);
                }
                if (name === "queued" || name === "fixing" || name === "pr_open" || name === "pr_review") {
                  if (!ov) {
                    acts.push(`<button class="ov-btn" onclick="budgetOverride(${f.id},'once')">Run 1</button>`);
                    acts.push(`<button class="ov-btn" onclick="budgetOverride(${f.id},'exempt')">Exempt</button>`);
                  } else {
                    acts.push(`<button class="ov-btn" onclick="clearOverride(${f.id})">Clear</button>`);
                  }
                }
                const attn = name === "pr_review" && f.needs_attention ? ` <span class="attn">${esc(f.needs_attention)}</span>` : "";
                return `<div class="item">
        #${f.id} ${esc(f.summary)}${ovBadge}${attn}
        <div class="m">${esc(f.file || "")}${f.pr_url ? ` \u00b7 <a href="${esc(f.pr_url)}" target="_blank">PR</a>` : ""}${acts.length ? " " + acts.join(" ") : ""}</div>
      </div>`;
              },
            )
            .join("")
        : '<div class="empty">\u2014</div>';
      return `<div class="col"><h3>${name} (${items.length})</h3>${body}</div>`;
    })
    .join("");
}

function renderAllFindings(all: Finding[], filtered: Finding[]): void {
  $("nFindings").textContent = `(${filtered.length}/${all.length})`;
  $("allFindings").innerHTML = filtered.length
    ? filtered.map((f) => findingCard(f, false)).join("")
    : '<div class="empty">' +
      (all.length ? "all filtered out" : "no findings yet") +
      "</div>";
}

function renderRepos(repos: Repo[]): void {
  if (!repos.length) {
    $("repos").innerHTML = '<div class="empty">no repos configured</div>';
    return;
  }
  $("repos").innerHTML = repos
    .map(
      (r) => `<div class="repo-row">
      <span class="name">${esc(r.name)}${r.enabled ? "" : ' <span class="paused">PAUSED</span>'}</span>
      <span class="url"><a href="${esc(r.url)}" target="_blank">${esc(r.url)}</a></span>
      <span class="meta">${esc(r.forge)} \u00b7 ${esc(r.default_branch)}${r.last_hunt_at ? " \u00b7 hunted " + ts(r.last_hunt_at) : ""}</span>
      <button class="${r.enabled ? "uq" : "q"}" onclick="toggleRepo(${r.id},${r.enabled ? "false" : "true"})">${r.enabled ? "Pause" : "Resume"}</button>
      <button class="r" data-name="${esc(r.name)}" onclick="removeRepo(${r.id},this.dataset.name)">Remove</button>
      <details class="repo-notes" id="rn-${r.id}" ontoggle="toggleRepoNotes(${r.id},this.open)">
        <summary>Notes</summary>
        <div class="repo-notes-body" id="rn-body-${r.id}"></div>
      </details>
    </div>`,
    )
    .join("");
}

function renderJobs(jobs: Job[]): void {
  if (!jobs.length) {
    $("jobs").innerHTML = '<div class="empty">no jobs yet</div>';
    return;
  }
  $("jobs").innerHTML = `<table>
    <tr><th>id</th><th>kind</th><th>repo</th><th>state</th>
        <th class="num">tokens</th><th class="num">calls</th><th class="num">dur</th><th>killed</th></tr>
    ${jobs
      .map(
        (j) => `<tr>
      <td>${j.id}</td><td>${esc(j.kind)}</td><td>${esc(j.repo_name)}</td>
      <td class="state-${esc(j.state)}">${esc(j.state)}</td>
      <td class="num">${ktok(j.tokens_new)}</td>
      <td class="num">${j.calls ?? "\u2013"}</td>
      <td class="num">${dur(j)}</td>
      <td>${esc(j.killed_reason || "")}</td>
    </tr>`,
      )
      .join("")}
  </table>`;
}

function renderEvents(events: Event[]): void {
  const evs = events.slice(0, 30);
  if (!evs.length) {
    $("events").innerHTML = '<div class="empty">quiet</div>';
    return;
  }
  $("events").innerHTML = evs
    .map(
      (e) =>
        `<div class="ev"><span class="t">${ts(e.at)}</span> <span class="k">${esc(e.kind)}</span> ${esc(e.message)}</div>`,
    )
    .join("");
  const last = evs[0];
  $("lastEvent").textContent = `${ts(last.at)} ${last.kind}: ${last.message}`;
}

function pct(v: number | null): string {
  return v != null ? (v * 100).toFixed(2) + "%" : "\u2013";
}

function renderStats(stats: Stats): void {
  const t = stats.totals;
  let html = `<div style="margin:8px 0;font-size:12px">
    <b>Totals:</b> ${t.jobs} jobs \u00b7 ${ktok(t.total_tokens)} tokens
    \u00b7 ${t.total_calls ?? 0} calls \u00b7 ${t.done} done \u00b7 ${t.denied} denied
    \u00b7 usage: ${pct(t.total_usage_delta)}
  </div>`;

  if (stats.by_kind.length) {
    html += `<table>
      <tr><th>kind</th><th class="num">jobs</th><th class="num">done</th>
          <th class="num">failed</th><th class="num">killed</th><th class="num">denied</th>
          <th class="num">tokens</th><th class="num">avg</th>
          <th class="num">usage \u0394</th><th>models</th></tr>
      ${stats.by_kind
        .map(
          (k) => `<tr>
        <td>${esc(k.kind)}</td>
        <td class="num">${k.jobs}</td><td class="num">${k.done}</td>
        <td class="num">${k.failed}</td><td class="num">${k.killed}</td>
        <td class="num">${k.denied}</td>
        <td class="num">${ktok(k.total_tokens)}</td>
        <td class="num">${ktok(k.avg_tokens)}</td>
        <td class="num">${pct(k.total_usage_delta)}</td>
        <td>${esc(k.models || "")}</td>
      </tr>`,
        )
        .join("")}
    </table>`;
  }

  if (stats.by_finding.length) {
    html += `<div style="margin-top:10px"><b>Per finding:</b></div><table>
      <tr><th>#</th><th>fingerprint</th><th>status</th><th>sev</th>
          <th class="num">jobs</th><th class="num">tokens</th>
          <th class="num">calls</th><th class="num">usage \u0394</th></tr>
      ${stats.by_finding
        .map(
          (f) => `<tr>
        <td>${f.finding_id}</td><td class="fp">${esc(f.fingerprint)}</td>
        <td>${esc(f.status)}</td><td>${esc(f.severity)}</td>
        <td class="num">${f.jobs}</td><td class="num">${ktok(f.total_tokens)}</td>
        <td class="num">${f.total_calls ?? "\u2013"}</td>
        <td class="num">${pct(f.total_usage_delta)}</td>
      </tr>`,
        )
        .join("")}
    </table>`;
  }

  $("stats").innerHTML = html || '<div class="empty">no job data yet</div>';
}

// ---------------------------------------------------------------------------
// Main refresh loop
// ---------------------------------------------------------------------------

async function refresh(): Promise<void> {
  try {
    const [summary, findings, jobs, events, stats] = await Promise.all([
      api<Summary>("/api/summary"),
      api<Finding[]>("/api/findings"),
      api<Job[]>("/api/jobs"),
      api<Event[]>("/api/events"),
      api<Stats>("/api/stats"),
    ]);
    if (
      [summary, findings, jobs, events, stats].some(
        (r) => r.status !== 200,
      )
    ) {
      throw new Error("api error");
    }
    // Re-verify the actual bytes that came back against SummarySchema --
    // TypeScript's `api<Summary>(...)` cast above is compile-time only
    // and trusts the wire unconditionally; this is the runtime half,
    // mirroring hunter.server._validate_summary on the Python side.
    // Throws (caught below) on any mismatch instead of silently
    // rendering whatever shape came back -- e.g. a wrong-typed field
    // that used to just print "NaNs" with no error at all.
    const s = SummarySchema.parse(summary.body);
    const all = findings.body!;

    // Snapshot open <details> elements before DOM rebuild.
    const openDetails = new Set<string>();
    for (const d of document.querySelectorAll("details[open]")) {
      const card = d.closest(".card")?.querySelector(".fp")?.textContent ?? d.id ?? d.parentElement?.id ?? "";
      const label = d.querySelector("summary")?.textContent ?? "";
      if (card) openDetails.add(card + "|" + label);
    }

    renderWindows(s.backend_status_html);
    renderActivity(s);

    // ---- populate filter dropdowns (preserve selection) ----
    const inbox = all.filter((f) => f.status === "new");
    const repos = [
      ...new Set(all.map((f) => f.fingerprint.split(":")[0])),
    ].sort();
    const inboxClasses = ([
      ...new Set(inbox.map((f) => f.category || f.bug_class).filter(c => c != null && c !== "")),
    ] as string[]).sort();
    const allClasses = ([
      ...new Set(all.map((f) => f.category || f.bug_class).filter(c => c != null && c !== "")),
    ] as string[]).sort();
    populateCheckboxGroup("fRepo", repos);
    populateCheckboxGroup("fClass", inboxClasses);
    populateCheckboxGroup("pfRepo", repos);
    populateCheckboxGroup("pfClass", allClasses);
    populateCheckboxGroup("afRepo", repos);
    populateCheckboxGroup("afClass", allClasses);

    // ---- apply filters (same filter bar, three independent instances) ----
    const filtered = applyFindingFilters(inbox, "f");
    $("nInbox").textContent = `(${filtered.length}/${inbox.length})`;
    $("inbox").innerHTML = filtered.length
      ? filtered.map((f) => findingCard(f, true)).join("")
      : '<div class="empty">' +
        (inbox.length ? "all filtered out" : "inbox zero") +
        "</div>";

    renderPipeline(applyFindingFilters(all, "pf"));
    renderAllFindings(all, applyFindingFilters(all, "af"));
    renderRepos(s.repos || []);

    const supp = all.filter(
      (f) => f.status === "rejected" || f.status === "wontfix",
    );
    $("nSupp").textContent = `(${supp.length})`;
    $("suppressed").innerHTML = supp.length
      ? supp
          .map(
            (f) => `<div class="card">
          <div class="top"><span class="badge">${esc(f.status)}</span>
          <span class="fp">#${f.id} ${esc(f.fingerprint)}</span></div>
          <div class="sum">${esc(f.summary)}</div>
          <div class="loc">${esc(f.verdict_reason || "(no reason)")}</div>
        </div>`,
          )
          .join("")
      : '<div class="empty">nothing suppressed</div>';

    const notes = all.filter((f) => f.status === "note");
    $("nNotes").textContent = `(${notes.length})`;
    $("notes").innerHTML = notes.length
      ? notes
          .map(
            (f) => `<div class="card">
          <div class="top"><span class="badge">note</span>
          <span class="fp">#${f.id} ${esc(f.fingerprint)}</span></div>
          <div class="sum">${esc(f.summary)}</div>
          <div class="loc">${esc(f.file || "")}${f.line ? ":" + f.line : ""}</div>
          <div class="acts">
            <button onclick="verdict(${f.id},'queued',false)">Queue fix</button>
            <button onclick="verdict(${f.id},'rejected',true)">Reject</button>
          </div>
        </div>`,
          )
          .join("")
      : '<div class="empty">no notes</div>';

    renderStats(stats.body!);
    renderJobs(jobs.body!);
    renderEvents(events.body!);

    // Restore open <details> elements after DOM rebuild.
    for (const d of document.querySelectorAll("details")) {
      const card = d.closest(".card")?.querySelector(".fp")?.textContent ?? d.id ?? d.parentElement?.id ?? "";
      const label = d.querySelector("summary")?.textContent ?? "";
      if (card && openDetails.has(card + "|" + label)) d.open = true;
    }

    const btn = $button("runCycle");
    if (s.cycle_running) {
      btn.textContent = "cycle running\u2026";
      btn.disabled = true;
    } else if (btn.textContent === "cycle running\u2026") {
      btn.textContent = "Run Cycle";
      btn.disabled = false;
    }
    $("err").style.display = "none";
  } catch (e) {
    $("err").textContent = "refresh failed: " + e;
    $("err").style.display = "block";
  }
}

// ---------------------------------------------------------------------------
// Left nav / page routing
// ---------------------------------------------------------------------------

const NAV_PAGES = ["status", "inbox", "pipeline", "findings", "repos", "stats", "log"];

function showPage(name: string): void {
  const page = NAV_PAGES.includes(name) ? name : "inbox";
  for (const el of document.querySelectorAll<HTMLElement>(".page")) {
    el.classList.toggle("active", el.id === `page-${page}`);
  }
  for (const el of document.querySelectorAll<HTMLElement>("#nav .nav-item")) {
    el.classList.toggle("active", el.dataset.page === page);
  }
  if (location.hash.slice(1) !== page) location.hash = page;
}

for (const el of document.querySelectorAll<HTMLElement>("#nav .nav-item")) {
  el.onclick = () => showPage(el.dataset.page ?? "inbox");
}
window.addEventListener("hashchange", () => showPage(location.hash.slice(1)));
showPage(location.hash.slice(1));

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

$button("runCycle").addEventListener("click", runCycle);

// Mount the three filter bars (Inbox reproduces its original "f" ids
// byte-for-byte -- see filterBarHtml's comment), then wire every
// select/range in each to trigger a refresh on change.
$("filters-inbox").innerHTML = filterBarHtml("f", false);
$("filters-pipeline").innerHTML = filterBarHtml("pf", false);
$("filters-findings").innerHTML = filterBarHtml("af", true);

for (const p of ["f", "pf", "af"]) {
  // Wire checkbox groups
  for (const suffix of ["Status", "Repo", "Type", "Class", "Sev"]) {
    wireCheckboxGroup(`${p}${suffix}`, refresh);
  }
  // Sort stays a <select>
  const sortEl = document.getElementById(`${p}Sort`);
  if (sortEl) (sortEl as HTMLSelectElement).onchange = refresh;
  // Confidence slider
  const confInput = document.getElementById(`${p}Conf`) as HTMLInputElement | null;
  if (confInput) {
    confInput.oninput = () => {
      const label = document.getElementById(`${p}ConfVal`);
      if (label) label.textContent = confInput.value + "%";
      refresh();
    };
  }
}

refresh();
setInterval(refresh, 5000);
