// Idle-Token Bug Hunter — triage dashboard

// ---------------------------------------------------------------------------
// API types (mirror hunter.types / hunter.store)
// ---------------------------------------------------------------------------

interface WindowInfo {
  used_fraction: number | null;
  status: string | null;
  resets_at: number | null;
  age_s: number;
  stale: boolean;
  ramp: number | null;
}

interface Finding {
  type: string;  // 'bug' | 'dep_update' | 'test_gap' | 'refactor'
  id: number;
  repo_id: number;
  fingerprint: string;
  file: string;
  symbol: string | null;
  line: number | null;
  category: string;  // bug_class | update_type | 'coverage' | smell_type
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

interface Job {
  id: number;
  kind: string;
  repo_id: number;
  repo_name: string;
  finding_id: number | null;
  state: string;
  tokens_new: number | null;
  calls: number | null;
  exit_code: number | null;
  killed_reason: string | null;
  started_at: number | null;
  finished_at: number | null;
}

interface Event {
  id: number;
  at: number;
  kind: string;
  message: string;
  job_id: number | null;
  finding_id: number | null;
}

interface Repo {
  id: number;
  name: string;
  url: string;
  path: string;
  forge: string;
  default_branch: string;
  last_hunt_sha: string | null;
  last_hunt_at: number | null;
  enabled: number;
  added_at: number;
}

interface CurrentJob extends Job {
  finding_summary?: string | null;
  finding_fingerprint?: string | null;
}

interface NextCandidate {
  kind: string;
  id: number;
  label: string | null;
  is_finding: boolean;
  budget_state: string; // "allowed" | "denied" | "exempt"
  budget_reason: string;
  budget_retry_at: number | null;
}

interface SchedulerState {
  state: string; // "idle" | "denied" | "error"
  detail: string;
  next_wake_at: number | null;
  updated_at: number;
}

interface Summary {
  windows: Record<string, WindowInfo>;
  counts: Record<string, number>;
  type_counts: Record<string, number>;
  repos: Repo[];
  last_cycle: Event | null;
  cycle_running: boolean;
  current_job: CurrentJob | null;
  next_candidate: NextCandidate | null;
  scheduler_state: SchedulerState | null;
}

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

function dur(j: Job): string {
  if (!j.started_at) return "\u2013";
  const end = j.finished_at || Date.now();
  return Math.round((end - j.started_at) / 1000) + "s";
}

// ---------------------------------------------------------------------------
// Filter dropdown management
// ---------------------------------------------------------------------------

function populateSelect(id: string, values: string[]): void {
  const el = $select(id);
  const prev = el.value;
  const existing = new Set(
    [...el.options].slice(1).map((o) => o.value),
  );
  const wanted = new Set(values);
  for (const v of values) {
    if (!existing.has(v)) {
      const o = document.createElement("option");
      o.value = o.textContent = v;
      el.appendChild(o);
    }
  }
  for (const o of [...el.options].slice(1)) {
    if (!wanted.has(o.value)) o.remove();
  }
  el.value = wanted.has(prev) ? prev : "";
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
});

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

function renderWindows(windows: Record<string, WindowInfo>): void {
  const keys = Object.keys(windows).sort();
  if (!keys.length) {
    $("windows").innerHTML =
      '<span class="empty">no window data</span>';
    return;
  }
  $("windows").innerHTML = keys
    .map((k) => {
      const w = windows[k];
      const pct =
        w.used_fraction == null
          ? null
          : Math.min(100, Math.round(w.used_fraction * 100));
      const rampPct =
        w.ramp == null ? null : Math.min(100, Math.round(w.ramp * 100));
      const availPct =
        pct == null || rampPct == null ? null : Math.max(0, rampPct - pct);
      const cls = w.stale
        ? "stale"
        : w.status === "exhausted" || (pct !== null && pct >= 100)
          ? "bad"
          : "ok";
      const label = k.replace(/^anthropic:/, "");
      const avail = availPct == null ? "" : ` \u00b7 ${availPct}% avail`;
      const marker =
        rampPct == null ? "" : `<i class="ramp" style="left:${rampPct}%"></i>`;
      return `<div class="win">
      <div class="lab"><b>${esc(label)}</b><span>${pct == null ? "?" : pct + "% used"}${avail}${w.stale ? " \u26a0stale" : ""}</span></div>
      <div class="bar"><i class="${cls}" style="width:${pct ?? 0}%"></i>${marker}</div>
      <div class="sub">resets ${ts(w.resets_at)}${w.resets_at ? " (" + countdown(w.resets_at) + ")" : ""} \u00b7 probed ${Math.round(w.age_s / 60)}m ago</div>
    </div>`;
    })
    .join("");
}

function renderActivity(s: Summary): void {
  const cj = s.current_job;
  if (cj) {
    const label =
      cj.finding_id != null
        ? `#${cj.finding_id} ${esc(cj.finding_summary || cj.finding_fingerprint || "")}`
        : esc(cj.repo_name);
    $("activity").innerHTML =
      `<div class="row"><b class="running">\u25b6 running</b> ${esc(cj.kind)}: ${label}` +
      ` <span class="dim">(${dur(cj)}, job #${cj.id})</span></div>`;
    return;
  }
  const rows: string[] = [];
  const ss = s.scheduler_state;
  if (ss) {
    rows.push(
      `<div class="row"><b class="${esc(ss.state)}">\u23f8 ${esc(ss.state)}</b> ${esc(ss.detail)}</div>`,
    );
    if (ss.next_wake_at) {
      rows.push(
        `<div class="row dim">next check ~${countdown(ss.next_wake_at)} (${ts(ss.next_wake_at)})</div>`,
      );
    }
  } else {
    rows.push('<div class="row dim">warming up \u2014 no cycle has run yet</div>');
  }
  const nc = s.next_candidate;
  if (nc) {
    const label = nc.is_finding ? `#${nc.id} ${esc(nc.label || "")}` : esc(nc.label || "");
    const reason = nc.budget_state === "denied" ? ` (${esc(nc.budget_reason)})` : "";
    rows.push(
      `<div class="row dim">next up: ${esc(nc.kind)} ${label}` +
        ` \u00b7 budget: <span class="b-${esc(nc.budget_state)}">${esc(nc.budget_state)}</span>${reason}</div>`,
    );
    if (nc.budget_state === "denied" && nc.budget_retry_at) {
      rows.push(
        `<div class="row dim">budget available ~${countdown(nc.budget_retry_at)} (${ts(nc.budget_retry_at)})</div>`,
      );
    }
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
  };
  const typeLabel = typeLabels[f.type] || f.type || "?";
  const category = f.category || f.bug_class || "";
  
  return `<div class="card">
    <div class="top">
      <span class="badge type-${f.type || 'bug'}">${typeLabel}</span>
      <span class="badge sev-${sev}">${sev} \u00b7 ${conf}</span>
      <span class="badge">${esc(category)}</span>
      <span class="fp">#${f.id} ${esc(f.fingerprint)}</span>
    </div>
    <div class="sum">${esc(f.summary)}</div>
    <div class="loc">${loc}${f.introduced_by ? " \u00b7 introduced by " + esc(f.introduced_by) : ""}</div>
    ${
      detail || plan
        ? `<details><summary>detail + evidence plan</summary>
      ${detail ? `<pre>${esc(detail)}</pre>` : ""}
      ${plan ? `<pre>evidence plan:\n${esc(plan)}</pre>` : ""}
    </details>`
        : ""
    }
    ${timeline}
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
    const s = summary.body!;
    const all = findings.body!;

    // Snapshot open <details> elements before DOM rebuild.
    const openDetails = new Set<string>();
    for (const d of document.querySelectorAll("details[open]")) {
      const card = d.closest(".card")?.querySelector(".fp")?.textContent ?? d.id ?? d.parentElement?.id ?? "";
      const label = d.querySelector("summary")?.textContent ?? "";
      if (card) openDetails.add(card + "|" + label);
    }

    renderWindows(s.windows || {});
    renderActivity(s);

    // ---- populate filter dropdowns (preserve selection) ----
    const inbox = all.filter((f) => f.status === "new");
    const repos = [
      ...new Set(all.map((f) => f.fingerprint.split(":")[0])),
    ].sort();
    const classes = ([
      ...new Set(inbox.map((f) => f.category || f.bug_class).filter(c => c != null && c !== "")),
    ] as string[]).sort();
    populateSelect("fRepo", repos);
    populateSelect("fClass", classes);

    // ---- apply filters ----
    const fRepo = $select("fRepo").value;
    const fType = $select("fType").value;
    const fClass = $select("fClass").value;
    const fSev = $select("fSev").value;
    const fConf = parseInt($input("fConf").value, 10) / 100;
    const fSort = $select("fSort").value;

    let filtered = inbox.filter((f) => {
      if (fRepo && !f.fingerprint.startsWith(fRepo + ":"))
        return false;
      if (fType && f.type !== fType) return false;
      if (fClass && (f.category || f.bug_class) !== fClass) return false;
      if (
        fSev &&
        (SEV_RANK[f.severity] || 0) < (SEV_RANK[fSev] || 0)
      )
        return false;
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
      filtered.sort(
        (a, b) => (b.created_at || 0) - (a.created_at || 0),
      );
    } else if (fSort === "oldest") {
      filtered.sort(
        (a, b) => (a.created_at || 0) - (b.created_at || 0),
      );
    } else if (fSort === "repo") {
      filtered.sort((a, b) =>
        a.fingerprint.localeCompare(b.fingerprint),
      );
    }

    $("nInbox").textContent = `(${filtered.length}/${inbox.length})`;
    $("inbox").innerHTML = filtered.length
      ? filtered.map((f) => findingCard(f, true)).join("")
      : '<div class="empty">' +
        (inbox.length ? "all filtered out" : "inbox zero") +
        "</div>";

    renderPipeline(all);
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

const NAV_PAGES = ["status", "inbox", "pipeline", "repos", "stats", "log"];

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

for (const id of ["fRepo", "fClass", "fSev", "fSort"]) {
  $(id).onchange = refresh;
}
const confInput = $input("fConf");
confInput.oninput = () => {
  $("fConfVal").textContent = confInput.value + "%";
  refresh();
};

refresh();
setInterval(refresh, 5000);
