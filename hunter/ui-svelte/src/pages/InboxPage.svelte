<script lang="ts">
  import { store } from "../lib/api.svelte";
  import type { Finding } from "../lib/types";
  import FilterBar from "../components/FilterBar.svelte";
  import FindingCard from "../components/FindingCard.svelte";

  // Inbox shows only status=new findings
  const inbox = $derived(store.findings.filter((f) => f.status === "new"));

  let displayed = $state<Finding[]>([]);
  const repoNames = $derived(new Map((store.summary?.repos ?? []).map((r) => [r.id, r.name])));
</script>

<div class="page-enter">
  <h2 class="page-title">
    INBOX
    <span class="count-badge">{displayed.length}<span class="count-sep">/</span>{inbox.length}</span>
  </h2>

  <FilterBar
    findings={inbox}
    prefix="f"
    showStatus={false}
    repoNames={repoNames}
    onFilter={(f: Finding[]) => { displayed = f; }}
  />

  {#if displayed.length === 0}
    <div class="empty-state">
      <div class="empty-icon">◇</div>
      <p class="empty-title">Inbox clear</p>
      <p class="empty-sub">No new findings to triage. Check back after the next hunt cycle.</p>
    </div>
  {:else}
    <div class="card-list">
      {#each displayed as finding (finding.id)}
        <FindingCard {finding} actions={true} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
    margin-bottom: 1rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .count-badge {
    font-size: 0.75rem;
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.05);
    padding: 0.0625rem 0.5rem;
    border-radius: 99px;
  }
  .count-sep { opacity: 0.4; margin: 0 0.0625rem; }

  .card-list {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .card-list > :global(.card) {
    margin-bottom: 0;
  }

  .empty-state {
    text-align: center;
    padding: 3rem 1rem;
    color: var(--text-dim);
  }
  .empty-icon {
    font-size: 2rem;
    opacity: 0.25;
    margin-bottom: 0.75rem;
  }
  .empty-title {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text);
    margin-bottom: 0.25rem;
  }
  .empty-sub {
    font-size: 0.8125rem;
    max-width: 28rem;
    margin: 0 auto;
    line-height: 1.5;
  }
</style>
