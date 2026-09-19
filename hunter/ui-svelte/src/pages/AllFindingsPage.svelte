<script lang="ts">
  import { onMount } from "svelte";
  import { store } from "../lib/api.svelte";
  import type { Finding } from "../lib/types";
  import FilterBar from "../components/FilterBar.svelte";
  import FindingCard from "../components/FindingCard.svelte";

  let { focusId = null }: { focusId?: number | null } = $props();

  let displayed = $state<Finding[]>([]);
  let filterBar: { clearFilters: () => void } | undefined = $state();
  const repoNames = $derived(new Map((store.summary?.repos ?? []).map((r) => [r.id, r.name])));

  onMount(() => {
    if (focusId == null) return;
    const target = `finding-${focusId}`;
    let clearedFilters = false;
    const interval = setInterval(() => {
      const el = document.getElementById(target);
      if (el) {
        clearInterval(interval);
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.classList.add("highlight-focus");
        return;
      }
      // Finding exists in data but not rendered → filters are hiding it.
      if (!clearedFilters && store.findings.some((f) => f.id === focusId)) {
        clearedFilters = true;
        filterBar?.clearFilters();
      }
    }, 50);
    setTimeout(() => clearInterval(interval), 5000);
  });
</script>

<div class="page-enter">
  <h2 class="page-title">
    ALL FINDINGS
    <span class="count-badge">{displayed.length}<span class="count-sep">/</span>{store.findings.length}</span>
  </h2>

  <FilterBar
    bind:this={filterBar}
    findings={store.findings}
    prefix="a"
    showStatus={true}
    repoNames={repoNames}
    onFilter={(f: Finding[]) => { displayed = f; }}
  />

  {#if displayed.length === 0}
    <div class="empty-state">
      <div class="empty-icon">⊘</div>
      <p class="empty-title">No matches</p>
      <p class="empty-sub">No findings match the current filters. Try broadening your criteria.</p>
    </div>
  {:else}
    <div class="card-list">
      {#each displayed as finding (finding.id)}
        <FindingCard {finding} actions={false} />
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
  :global(.highlight-focus) {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    box-shadow: 0 0 12px rgba(78, 168, 222, 0.3), inset 0 0 0 1px rgba(78, 168, 222, 0.15);
  }
</style>
