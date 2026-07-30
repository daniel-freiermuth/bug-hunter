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
}

interface Finding {
  id: number;
  repo_id: number;
  fingerprint: string;
  file: string;
  symbol: string | null;
  line: number | null;
  bug_class: string;
  severity: string;
  confidence: number;
  summary: string;
  detail: string | null;
  evidence_plan: string | null;
  introduced_by: string | null;
  status: string;
  verdict_reason: string | null;
  pr_url: string | null;
  created_at: number;
  updated_at: number;
  timeline: Event[];
  budget_override: string | null;
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

interface Summary {
  windows: Record<string, WindowInfo>;
  counts: Record<string, number>;
  repos: unknown[];
  last_cycle: Event | null;
  cycle_running: boolean;
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

// Expose to onclick handlers in rendered HTML
Object.assign(window, { verdict, recheck, unqueue, budgetOverride, clearOverride, clearAllOverrides });

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
      const cls = w.stale
        ? "stale"
        : w.status === "exhausted" || (pct !== null && pct >= 100)
          ? "bad"
          : "ok";
      const label = k.replace(/^anthropic:/, "");
      return `<div class="win">
      <div class="lab"><b>${esc(label)}</b><span>${pct == null ? "?" : pct + "%"}${w.stale ? " \u26a0stale" : ""}</span></div>
      <div class="bar"><i class="${cls}" style="width:${pct ?? 0}%"></i></div>
      <div class="sub">resets ${ts(w.resets_at)}${w.resets_at ? " (" + countdown(w.resets_at) + ")" : ""} \u00b7 probed ${Math.round(w.age_s / 60)}m ago</div>
    </div>`;
    })
    .join("");
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
  return `<div class="card">
    <div class="top">
      <span class="badge sev-${sev}">${sev} \u00b7 ${conf}</span>
      <span class="badge">${esc(f.bug_class || "")}</span>
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
    ["pr_open", (f) => f.status === "pr_open"],
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
                if (name === "queued" || name === "fixing" || name === "pr_open") {
                  if (!ov) {
                    acts.push(`<button class="ov-btn" onclick="budgetOverride(${f.id},'once')">Run 1</button>`);
                    acts.push(`<button class="ov-btn" onclick="budgetOverride(${f.id},'exempt')">Exempt</button>`);
                  } else {
                    acts.push(`<button class="ov-btn" onclick="clearOverride(${f.id})">Clear</button>`);
                  }
                }
                return `<div class="item">
        #${f.id} ${esc(f.summary)}${ovBadge}
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
      const card = d.closest(".card")?.querySelector(".fp")?.textContent ?? d.parentElement?.id ?? "";
      const label = d.querySelector("summary")?.textContent ?? "";
      if (card) openDetails.add(card + "|" + label);
    }

    renderWindows(s.windows || {});

    // ---- populate filter dropdowns (preserve selection) ----
    const inbox = all.filter((f) => f.status === "new");
    const repos = [
      ...new Set(all.map((f) => f.fingerprint.split(":")[0])),
    ].sort();
    const classes = [
      ...new Set(inbox.map((f) => f.bug_class).filter(Boolean)),
    ].sort();
    populateSelect("fRepo", repos);
    populateSelect("fClass", classes);

    // ---- apply filters ----
    const fRepo = $select("fRepo").value;
    const fClass = $select("fClass").value;
    const fSev = $select("fSev").value;
    const fConf = parseInt($input("fConf").value, 10) / 100;
    const fSort = $select("fSort").value;

    let filtered = inbox.filter((f) => {
      if (fRepo && !f.fingerprint.startsWith(fRepo + ":"))
        return false;
      if (fClass && f.bug_class !== fClass) return false;
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
      const card = d.closest(".card")?.querySelector(".fp")?.textContent ?? d.parentElement?.id ?? "";
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
