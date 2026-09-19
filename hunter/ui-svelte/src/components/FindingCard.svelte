<script lang="ts">
  import { store, post } from "../lib/api.svelte";
  import type { Finding } from "../lib/types";
  import { pct, datetime, typeLabel } from "../lib/format";
  import FindingDetail from "./FindingDetail.svelte";

  let { finding, actions = false }: { finding: Finding; actions?: boolean } = $props();

  let expanded = $state(false);
  let reasonText = $state("");
  let showReasonPrompt = $state<string | null>(null); // "rejected" | "wontfix" | null
  let busy = $state(false);

  function statusBadgeClass(status: string): string {
    switch (status) {
      case "new": return "status-new";
      case "queued": return "status-queued";
      case "rechecking": return "status-rechecking";
      case "fixing": return "status-fixing";
      case "pr_open": return "status-pr-open";
      case "merged": return "status-merged";
      case "rejected": return "status-rejected";
      case "wontfix": return "status-wontfix";
      case "note": return "status-note";
      default: return "status-default";
    }
  }

  function sevClass(sev: string): string {
    switch (sev) {
      case "high": return "sev-high";
      case "medium": return "sev-medium";
      case "low": return "sev-low";
      default: return "sev-default";
    }
  }

  async function doVerdict(status: string, reason?: string) {
    busy = true;
    await post("/api/verdict", { id: finding.id, status, reason: reason || undefined });
    await store.refresh();
    busy = false;
    showReasonPrompt = null;
    reasonText = "";
  }

  async function doRecheck() {
    busy = true;
    await post("/api/recheck", { id: finding.id });
    await store.refresh();
    busy = false;
  }

  async function doUnqueue() {
    busy = true;
    await post("/api/unqueue", { id: finding.id });
    await store.refresh();
    busy = false;
  }

  async function doOverride(mode: string | null) {
    busy = true;
    await post("/api/override", { id: finding.id, mode });
    await store.refresh();
    busy = false;
  }
</script>

<div class="card {sevClass(finding.severity)}" id="finding-{finding.id}">
  <!-- Header row: type pill + severity dot/text + confidence + category + status pill (right) -->
  <div class="card-header">
    <span class="type-pill" title={finding.type}>{typeLabel(finding.type)}</span>
    <span class="sev-indicator {sevClass(finding.severity)}">
      <span class="sev-dot"></span>
      {finding.severity}
    </span>
    <span class="confidence">{pct(finding.confidence)}</span>
    {#if finding.category}
      <span class="cat-label">{finding.category}</span>
    {/if}
    {#if finding.budget_override}
      <span class="override-badge" title="Budget override">⚡ {finding.budget_override}</span>
    {/if}
    {#if finding.needs_attention}
      <span class="attn-badge" title="Needs attention">⚠ {finding.needs_attention}</span>
    {/if}
    <span class="status-pill {statusBadgeClass(finding.status)}">
      {finding.status}
    </span>
  </div>

  <!-- Summary -->
  <p class="summary">{finding.summary}</p>

  <!-- Subtitle: fingerprint + file:line -->
  <div class="meta-line">
    <span class="fingerprint" title={finding.fingerprint}>{finding.fingerprint}</span>
    {#if finding.file}
      <span class="file-loc">
        {finding.file}{#if finding.line}:{finding.line}{/if}
      </span>
    {/if}
  </div>

  <!-- Timeline (collapsible) -->
  {#if finding.timeline && finding.timeline.length > 0}
    <details class="timeline">
      <summary class="timeline-toggle">
        Timeline ({finding.timeline.length})
      </summary>
      <div class="timeline-items">
        {#each finding.timeline as ev (ev.id)}
          <div class="tl-row">
            <span class="tl-dot"></span>
            <span class="tl-time">{datetime(ev.at)}</span>
            <span class="tl-kind">{ev.kind}</span>
            <span class="tl-msg">{ev.message}</span>
          </div>
        {/each}
      </div>
    </details>
  {/if}

  <!-- Actions (verdict / recheck / override) -->
  {#if actions}
    <div class="actions">
      {#if showReasonPrompt}
        <!-- Reason input for rejected/wontfix -->
        <div class="reason-row">
          <input
            type="text"
            class="reason-input"
            placeholder="Reason for {showReasonPrompt}…"
            bind:value={reasonText}
            onkeydown={(e: KeyboardEvent) => { if (e.key === "Enter" && !busy && reasonText.trim()) doVerdict(showReasonPrompt!, reasonText.trim()); }}
          />
          <button
            class="btn btn-confirm"
            disabled={busy || !reasonText.trim()}
            onclick={() => doVerdict(showReasonPrompt!, reasonText.trim())}
          >Confirm</button>
          <button
            class="btn btn-muted"
            onclick={() => { showReasonPrompt = null; reasonText = ""; }}
          >Cancel</button>
        </div>
      {:else}
        <div class="btn-row">
          <button class="btn btn-queue" disabled={busy} onclick={() => doVerdict("queued")} title="Queue for fix">Queue</button>
          <button class="btn btn-reject" disabled={busy} onclick={() => { showReasonPrompt = "rejected"; }} title="Reject">Reject</button>
          <button class="btn btn-muted" disabled={busy} onclick={() => { showReasonPrompt = "wontfix"; }} title="Won't fix">Wontfix</button>
          <button class="btn btn-muted" disabled={busy} onclick={() => doVerdict("note")} title="Mark as note">Note</button>
          <button class="btn btn-queue" disabled={busy} onclick={() => doVerdict("merged")} title="Mark as merged">Merged</button>

          {#if finding.status === "new"}
            <span class="sep"></span>
            <button class="btn btn-accent" disabled={busy} onclick={doRecheck} title="Queue for adversarial recheck">Recheck</button>
          {/if}

          {#if finding.status === "queued"}
            <span class="sep"></span>
            <button class="btn btn-warn" disabled={busy} onclick={doUnqueue} title="Remove from fix queue">Unqueue</button>
          {/if}

          <span class="sep"></span>
          {#if finding.budget_override}
            <button class="btn btn-muted" disabled={busy} onclick={() => doOverride(null)} title="Clear budget override">Clear override</button>
          {:else}
            <button class="btn btn-accent" disabled={busy} onclick={() => doOverride("once")} title="Budget override: once">⚡ Once</button>
            <button class="btn btn-accent" disabled={busy} onclick={() => doOverride("exempt")} title="Budget override: exempt">⚡ Exempt</button>
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  <!-- Expandable detail panel -->
  <button
    class="detail-toggle"
    onclick={() => { expanded = !expanded; }}
  >
    {expanded ? "▾ Hide detail" : "▸ Show detail"}
  </button>
  {#if expanded}
    <div class="detail-panel">
      {#if finding.detail}
        <p class="detail-text">{finding.detail}</p>
      {/if}
      {#if finding.evidence_plan}
        <div class="detail-section">
          <span class="detail-label">Evidence plan:</span>
          <p class="detail-text" style="margin-top:0.125rem">{finding.evidence_plan}</p>
        </div>
      {/if}
      {#if finding.pr_url}
        <div class="detail-section">
          <span class="detail-label">PR:</span>
          <a href={finding.pr_url} target="_blank" class="detail-link">{finding.pr_url}</a>
        </div>
      {/if}
      {#if finding.verdict_reason}
        <div class="detail-section">
          <span class="detail-label">Verdict reason:</span>
          <span class="detail-value">{finding.verdict_reason}</span>
        </div>
      {/if}
      <FindingDetail findingId={finding.id} />
    </div>
  {/if}
</div>

<style>
  /* ── Card ────────────────────────────────────────────────────── */
  .card {
    background: var(--bg-card);
    border-radius: var(--radius-md);
    padding: 0.75rem 0.875rem;
    margin-bottom: 0.5rem;
    border: 1px solid var(--border);
    border-left: 3px solid var(--text-dim);
    transition: transform var(--transition), box-shadow var(--transition),
                border-color var(--transition);
    overflow: hidden;
    min-width: 0;
  }
  .card:hover {
    transform: translateY(-1px);
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
    border-color: var(--border-hover);
  }
  .card.sev-high   { border-left-color: var(--sev-high); }
  .card.sev-medium { border-left-color: var(--sev-medium); }
  .card.sev-low    { border-left-color: var(--sev-low); }

  /* ── Header ─────────────────────────────────────────────────── */
  .card-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    font-size: 0.75rem;
  }

  .type-pill {
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    font-size: 0.6875rem;
    font-weight: 500;
    background: rgba(255, 255, 255, 0.06);
    color: var(--text-dim);
  }

  .sev-indicator {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    font-size: 0.6875rem;
    font-weight: 600;
    text-transform: uppercase;
  }
  .sev-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--text-dim);
  }
  .sev-indicator.sev-high   { color: var(--sev-high); }
  .sev-indicator.sev-high .sev-dot { background: var(--sev-high); }
  .sev-indicator.sev-medium { color: var(--sev-medium); }
  .sev-indicator.sev-medium .sev-dot { background: var(--sev-medium); }
  .sev-indicator.sev-low    { color: var(--sev-low); }
  .sev-indicator.sev-low .sev-dot { background: var(--sev-low); }

  .confidence {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  .cat-label {
    font-size: 0.6875rem;
    color: rgba(136, 136, 136, 0.7);
  }

  .override-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.3125rem;
    border-radius: 99px;
    background: rgba(78, 168, 222, 0.12);
    color: var(--accent);
  }

  .attn-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.3125rem;
    border-radius: 99px;
    background: rgba(238, 85, 68, 0.12);
    color: var(--sev-high);
  }

  .status-pill {
    margin-left: auto;
    padding: 0.0625rem 0.4375rem;
    border-radius: 99px;
    font-size: 0.625rem;
    font-weight: 500;
    letter-spacing: 0.02em;
  }
  .status-new, .status-rechecking { background: rgba(78, 168, 222, 0.12); color: var(--accent); }
  .status-queued, .status-fixing  { background: rgba(255, 153, 0, 0.12); color: var(--sev-medium); }
  .status-pr-open                 { background: rgba(68, 238, 136, 0.12); color: var(--ok); }
  .status-merged                  { background: rgba(68, 238, 136, 0.18); color: var(--ok); }
  .status-rejected                { background: rgba(238, 85, 68, 0.12); color: var(--bad); }
  .status-wontfix, .status-note   { background: rgba(136, 136, 136, 0.12); color: var(--text-dim); }
  .status-default                 { background: rgba(255, 255, 255, 0.06); color: var(--text-dim); }

  /* ── Summary ────────────────────────────────────────────────── */
  .summary {
    margin: 0.375rem 0 0;
    font-size: 0.9375rem;
    line-height: 1.5;
    overflow-wrap: break-word;
    word-break: break-word;
  }

  /* ── Meta (fingerprint + location) ──────────────────────────── */
  .meta-line {
    margin-top: 0.25rem;
    display: flex;
    gap: 0.75rem;
    font-size: 0.6875rem;
    color: var(--text-dim);
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    min-width: 0;
    overflow: hidden;
  }
  .fingerprint {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-loc {
    flex-shrink: 0;
    color: rgba(136, 136, 136, 0.7);
  }

  /* ── Timeline ───────────────────────────────────────────────── */
  .timeline {
    margin-top: 0.5rem;
  }

  .timeline-toggle {
    font-size: 0.6875rem;
    color: var(--text-dim);
    cursor: pointer;
    user-select: none;
  }
  .timeline-toggle:hover {
    color: var(--text);
  }

  .timeline-items {
    margin-top: 0.375rem;
    padding-left: 0.375rem;
    border-left: 1px solid rgba(255, 255, 255, 0.08);
    font-size: 0.6875rem;
    max-height: 10rem;
    overflow-y: auto;
  }

  .tl-row {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    color: var(--text-dim);
    padding: 0.1875rem 0;
    position: relative;
  }

  .tl-dot {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.2);
    flex-shrink: 0;
    position: relative;
    left: -0.625rem;
    margin-right: -0.375rem;
  }

  .tl-time {
    flex-shrink: 0;
    width: 7rem;
  }

  .tl-kind {
    font-weight: 600;
    color: var(--text);
    width: 4rem;
    flex-shrink: 0;
  }

  .tl-msg {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ── Actions bar ────────────────────────────────────────────── */
  .actions {
    margin-top: 0.5rem;
    padding-top: 0.5rem;
    border-top: 1px solid var(--border);
  }

  .reason-row {
    display: flex;
    gap: 0.375rem;
    align-items: center;
  }

  .reason-input {
    flex: 1;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.25rem 0.5rem;
    font-size: 0.75rem;
    color: var(--text);
  }
  .reason-input::placeholder {
    color: var(--text-dim);
  }
  .reason-input:focus {
    outline: none;
    border-color: var(--accent);
  }

  .btn-row {
    display: flex;
    gap: 0.3125rem;
    flex-wrap: wrap;
  }

  .btn {
    padding: 0.1875rem 0.4375rem;
    font-size: 0.6875rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .btn:disabled { opacity: 0.4; }

  .btn-queue { color: var(--text-dim); }
  .btn-queue:hover:not(:disabled) { background: rgba(68, 238, 136, 0.15); color: var(--ok); }

  .btn-reject { color: var(--text-dim); }
  .btn-reject:hover:not(:disabled) { background: rgba(238, 85, 68, 0.15); color: var(--bad); }

  .btn-muted:hover:not(:disabled) { background: rgba(255, 255, 255, 0.1); color: var(--text); }

  .btn-accent { color: var(--text-dim); }
  .btn-accent:hover:not(:disabled) { background: rgba(78, 168, 222, 0.15); color: var(--accent); }

  .btn-warn { color: var(--text-dim); }
  .btn-warn:hover:not(:disabled) { background: rgba(255, 153, 0, 0.15); color: var(--sev-medium); }

  .btn-confirm { background: rgba(238, 85, 68, 0.15); color: var(--bad); }
  .btn-confirm:hover:not(:disabled) { background: rgba(238, 85, 68, 0.25); }

  .sep {
    border-left: 1px solid var(--border);
    margin: 0 0.0625rem;
    align-self: stretch;
  }

  /* ── Detail toggle ──────────────────────────────────────────── */
  .detail-toggle {
    margin-top: 0.5rem;
    font-size: 0.6875rem;
    color: var(--accent);
    cursor: pointer;
    user-select: none;
    background: none;
    border: none;
    padding: 0;
  }
  .detail-toggle:hover {
    color: var(--accent-hover);
  }

  /* ── Detail panel (card-in-card) ────────────────────────────── */
  .detail-panel {
    margin-top: 0.375rem;
    background: var(--bg-panel);
    border-radius: var(--radius-md);
    padding: 0.625rem 0.75rem;
    border: 1px solid var(--border);
    box-shadow: inset 0 1px 3px rgba(0, 0, 0, 0.2);
    animation: fadeIn 150ms ease both;
  }

  .detail-text {
    font-size: 0.75rem;
    color: var(--text-dim);
    white-space: pre-wrap;
    margin-bottom: 0.5rem;
    line-height: 1.5;
  }

  .detail-section {
    font-size: 0.75rem;
    margin-bottom: 0.5rem;
  }

  .detail-label {
    color: var(--text-dim);
    font-weight: 600;
  }

  .detail-link {
    color: var(--accent);
    margin-left: 0.25rem;
  }
  .detail-link:hover {
    color: var(--accent-hover);
  }

  .detail-value {
    color: var(--text-dim);
    margin-left: 0.25rem;
  }
</style>
