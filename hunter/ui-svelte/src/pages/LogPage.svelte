<script lang="ts">
  import { store } from "../lib/api.svelte";
  import { ts, ktok, dur } from "../lib/format";

  let events = $derived(store.events);
  let jobs = $derived(store.jobs);

  // The daemon's event kinds (hunter-rs: `log_event(`). Ingest logs one kind
  // per finding type — `hunt` for bugs, the type's own name otherwise.
  function kindClass(kind: string): string {
    switch (kind) {
      // Work that landed.
      case "ship": return "kind-ok";
      case "error": return "kind-bad";
      // A new finding of any type is untriaged work, and a budget override is
      // a deliberate deviation: both want the eye.
      case "hunt":
      case "dep_update":
      case "test_gap":
      case "refactor":
      case "modernization":
      case "standards":
      case "override": return "kind-warn";
      // Nothing ran, or queued work was withdrawn.
      case "deny":
      case "unqueue": return "kind-stale";
      // Routine daemon and operator activity.
      case "cycle":
      case "fix":
      case "engage":
      case "harvest":
      case "recheck":
      case "verdict":
      case "repo": return "kind-accent";
      default: return "kind-default";
    }
  }

  function stateClass(state: string): string {
    switch (state) {
      case "done": return "state-ok";
      case "failed": return "state-bad";
      case "killed": return "state-warn";
      case "denied": return "state-stale";
      case "running": return "state-accent";
      default: return "state-default";
    }
  }
</script>

<div class="page-enter">
  <div class="page">
    <h2 class="page-title">Log</h2>

    <!-- Recent Events -->
    <div class="card">
      <h3 class="section-title">Recent Events</h3>
      {#if events.length === 0}
        <p class="empty">No events.</p>
      {:else}
        <div class="scroll-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th class="th-ts">Time</th>
                <th class="th-kind">Kind</th>
                <th>Message</th>
                <th class="th-links right">Links</th>
              </tr>
            </thead>
            <tbody>
              {#each events as ev (ev.id)}
                <tr>
                  <td class="cell-ts">{ts(ev.at)}</td>
                  <td>
                    <span class="badge {kindClass(ev.kind)}">
                      {ev.kind}
                    </span>
                  </td>
                  <td class="cell-msg" title={ev.message}>{ev.message}</td>
                  <td class="cell-links">
                    {#if ev.finding_id != null}
                      <a href="#findings:{ev.finding_id}" class="finding-link">
                        F#{ev.finding_id}
                      </a>
                    {/if}
                    {#if ev.job_id != null}
                      <span class="dim">J#{ev.job_id}</span>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>

    <!-- Recent Jobs -->
    <div class="card">
      <h3 class="section-title">Recent Jobs</h3>
      {#if jobs.length === 0}
        <p class="empty">No jobs.</p>
      {:else}
        <div class="scroll-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th class="th-ts">ID</th>
                <th class="th-kind">Kind</th>
                <th>Repo</th>
                <th class="th-kind">Finding</th>
                <th class="th-kind">State</th>
                <th class="right">Tokens</th>
                <th class="right">Calls</th>
                <th class="right">Duration</th>
                <th>Model</th>
              </tr>
            </thead>
            <tbody>
              {#each jobs as job (job.id)}
                <tr>
                  <td class="cell-ts">#{job.id}</td>
                  <td class="cell-kind">{job.kind}</td>
                  <td class="cell-small">{job.repo_name}</td>
                  <td class="cell-links">
                    <!-- A hunt has no finding of its own: it is the job
                         that produces them, so the column is empty rather
                         than linking somewhere misleading. -->
                    {#if job.finding_id != null}
                      <a href="#findings:{job.finding_id}" class="finding-link">
                        F#{job.finding_id}
                      </a>
                    {:else}
                      <span class="dim">–</span>
                    {/if}
                  </td>
                  <td>
                    <span class="badge {stateClass(job.state)}">
                      {job.state}
                    </span>
                  </td>
                  <td class="right mono">{ktok(job.tokens_new)}</td>
                  <td class="right mono">{job.calls ?? '–'}</td>
                  <td class="right mono">{dur(job.started_at, job.finished_at)}</td>
                  <td class="dim">{job.model ?? '–'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  /* ── Page ────────────────────────────────────────────────────── */
  .page {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
  }

  /* ── Card ────────────────────────────────────────────────────── */
  .card {
    background: var(--bg-card);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    overflow: hidden;
  }

  .section-title {
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 0.625rem 0.875rem 0.375rem;
  }

  .empty {
    color: var(--text-dim);
    font-size: 0.8125rem;
    padding: 0.5rem 0.875rem 0.75rem;
    animation: pulse 2s ease-in-out infinite;
  }

  /* ── Table ──────────────────────────────────────────────────── */
  .scroll-wrap {
    overflow-x: auto;
  }

  .data-table {
    width: 100%;
    font-size: 0.8125rem;
    border-collapse: collapse;
  }

  .data-table thead tr {
    border-bottom: 1px solid var(--border-hover);
    color: var(--text-dim);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .data-table th {
    text-align: left;
    padding: 0.4375rem 0.75rem;
    position: sticky;
    top: 0;
    background: var(--bg-card);
    z-index: 1;
  }

  .data-table tbody tr {
    border-bottom: 1px solid rgba(255, 255, 255, 0.03);
    transition: background-color var(--transition);
  }
  .data-table tbody tr:nth-child(even) {
    background: rgba(255, 255, 255, 0.015);
  }
  .data-table tbody tr:hover {
    background: rgba(255, 255, 255, 0.04);
  }

  .data-table td {
    padding: 0.375rem 0.75rem;
  }

  .right { text-align: right; }

  /* ── Cell types ──────────────────────────────────────────────── */
  .cell-ts {
    color: var(--text-dim);
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    white-space: nowrap;
    width: 7rem;
  }
  .th-ts { width: 7rem; }

  .cell-kind {
    color: var(--accent);
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.6875rem;
  }
  .th-kind { width: 6rem; }

  .cell-msg {
    font-size: 0.75rem;
    max-width: 32rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .cell-links {
    text-align: right;
    white-space: nowrap;
    font-size: 0.6875rem;
  }
  .th-links { width: 6rem; }

  .cell-small {
    font-size: 0.6875rem;
  }

  .mono {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.6875rem;
  }

  .dim {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  /* ── Badge ──────────────────────────────────────────────────── */
  .badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    font-weight: 500;
    white-space: nowrap;
  }

  .finding-link {
    color: var(--accent);
    margin-right: 0.375rem;
    font-size: 0.6875rem;
  }
  .finding-link:hover { color: var(--accent-hover); }

  /* kind badge colors */
  .kind-ok      { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .kind-bad     { background: rgba(238, 85, 68, 0.1); color: var(--bad); }
  .kind-warn    { background: rgba(255, 153, 0, 0.1); color: var(--sev-medium); }
  .kind-stale   { background: rgba(136, 136, 136, 0.1); color: var(--stale); }
  .kind-accent  { background: rgba(78, 168, 222, 0.1); color: var(--accent); }
  .kind-default { background: rgba(255, 255, 255, 0.04); color: var(--text-dim); }

  /* state badge colors */
  .state-ok      { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .state-bad     { background: rgba(238, 85, 68, 0.1); color: var(--bad); }
  .state-warn    { background: rgba(255, 153, 0, 0.1); color: var(--sev-medium); }
  .state-stale   { background: rgba(136, 136, 136, 0.1); color: var(--stale); }
  .state-accent  { background: rgba(78, 168, 222, 0.1); color: var(--accent); }
  .state-default { background: rgba(255, 255, 255, 0.04); color: var(--text-dim); }
</style>
