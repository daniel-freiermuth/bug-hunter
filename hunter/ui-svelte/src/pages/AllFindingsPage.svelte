<script lang="ts">
  import { store } from "../lib/api.svelte";
  import { navigation } from "../lib/navigation.svelte";
  import { filterFindings, hidingParams, type FilterScope } from "../lib/findingFilter";
  import { toast } from "../lib/toast.svelte";
  import { placeAtTop, settle } from "../lib/scroll";
  import FilterBar from "../components/FilterBar.svelte";
  import FindingCard from "../components/FindingCard.svelte";

  let { focusId = null }: { focusId?: number | null } = $props();

  const repoNames = $derived(new Map((store.summary?.repos ?? []).map((r) => [r.id, r.name])));
  const scope: FilterScope = $derived({ repoNames, showStatus: true });
  const displayed = $derived(filterFindings(store.findings, navigation.route.params, scope));

  // Each history entry places its finding once: later polls must not pull
  // the view back to it after the user has scrolled on, but a new visit to
  // the same finding (a new entry, on this still-mounted page) must.
  let placedEntry: string | null = null;
  let cancelPlace = () => {};
  $effect(() => () => cancelPlace());

  $effect(() => {
    const id = focusId;
    const entry = window.navigation.currentEntry?.key ?? null;
    // A jump still pending for another entry (one that cannot complete,
    // like a short list's last card) must not move this one.
    if (entry !== placedEntry) cancelPlace();
    if (id == null || entry === placedEntry) return;
    // Nothing to decide until the first poll has landed; summary and
    // findings arrive together.
    if (store.summary === null) return;
    const finding = store.findings.find((f) => f.id === id);
    if (!finding) {
      placedEntry = entry;
      toast(`F#${id} not found`, false);
      return;
    }
    // In the data but not in the list: filters hide it. Drop only those, so
    // the rest of the view survives. A link carries the filters of the view
    // it was made in, where the finding was visible; one that hides it now
    // means the finding changed since (status moved on, a recheck changed
    // confidence). Replacing the entry corrects the navigation that opened
    // the link, so Back does not land on the view that hid the finding. The
    // list follows the URL, and this runs again.
    if (!displayed.some((f) => f.id === id)) {
      const hiding = hidingParams(finding, navigation.route.params, scope);
      navigation.updateParams((params) => {
        for (const name of hiding) params.delete(name);
      }, true);
      return;
    }
    placedEntry = entry;
    // Effects run after the DOM update that rendered `displayed`. The cards
    // around it may still be placeholders, too short to scroll it to the
    // top yet: keep placing it while the list grows.
    const el = document.getElementById(`finding-${id}`);
    cancelPlace();
    if (el?.parentElement) cancelPlace = settle(el.parentElement, () => placeAtTop(el));
  });
</script>

<div class="page-enter">
  <h2 class="page-title">
    ALL FINDINGS
    <span class="count-badge">{displayed.length}<span class="count-sep">/</span>{store.findings.length}</span>
  </h2>

  <FilterBar
    findings={store.findings}
    prefix="a"
    showStatus={true}
    repoNames={repoNames}
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
        <FindingCard {finding} actions={true} focused={finding.id === focusId} />
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
