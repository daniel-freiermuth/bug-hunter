<script lang="ts">
  import type { FindingOut } from "../lib/types";
  import { SEV_RANK } from "../lib/format";
  import { Filter, needsSelector } from "../lib/filter.svelte";
  import { navigation } from "../lib/navigation.svelte";
  import { FILTER_KEYS, readConfidence, readSelection, readSort, writeSelection, type FilterKey } from "../lib/filterUrl";
  import MultiSelect from "./MultiSelect.svelte";

  let {
    findings,
    prefix,
    showStatus = false,
    repoNames = new Map<number, string>(),
    onFilter,
  }: {
    findings: FindingOut[];
    prefix: string;
    showStatus?: boolean;
    repoNames?: Map<number, string>;
    onFilter: (filtered: FindingOut[]) => void;
  } = $props();

  // -- Extract unique values from the findings list -------------------------
  const repos = $derived([...new Set(findings.map((f) => repoNames.get(f.repo_id) ?? "unknown"))].sort());
  const types = $derived([...new Set(findings.map((f) => f.type))].sort());
  const categories = $derived([...new Set(findings.filter((f) => f.category).map((f) => f.category!))].sort());
  const severities = $derived([...new Set(findings.map((f) => f.severity))].sort((a, b) => (SEV_RANK[b] ?? 0) - (SEV_RANK[a] ?? 0)));
  const statuses = $derived([...new Set(findings.map((f) => f.status))].sort());

  // Rebuild from the URL, never from the current options: filters survive
  // loading, polling, refreshes, and same-page browser history navigation.
  const repoFilter = $derived(new Filter(readSelection(navigation.route.params, "repo")));
  const typeFilter = $derived(new Filter(readSelection(navigation.route.params, "type")));
  const categoryFilter = $derived(new Filter(readSelection(navigation.route.params, "class")));
  const severityFilter = $derived(new Filter(readSelection(navigation.route.params, "severity")));
  const statusFilter = $derived(new Filter(readSelection(navigation.route.params, "status")));
  const minConfidence = $derived(readConfidence(navigation.route.params));
  const sortBy = $derived(readSort(navigation.route.params));

  function selectFilter(key: FilterKey, filter: Filter) {
    navigation.updateParams((params) => writeSelection(params, key, filter.selection));
  }

  function selectConfidence(value: number, replace: boolean) {
    navigation.updateParams((params) => {
      if (value === 0) params.delete("confidence");
      else params.set("confidence", String(value));
    }, replace);
  }

  // A slider drag is one history step, not one for every input event.
  let sliding = false;
  function changeConfidence(value: number) {
    selectConfidence(value, sliding);
    sliding = true;
  }

  export function clearFilters() {
    navigation.updateParams((params) => {
      for (const key of FILTER_KEYS) params.delete(key);
      params.delete("confidence");
    });
  }

  // -- Derived filtered + sorted output -------------------------------------
  const filtered = $derived.by(() => {
    let out = findings;

    out = out.filter((f) => repoFilter.accepts(repoNames.get(f.repo_id) ?? "unknown"));
    out = out.filter((f) => typeFilter.accepts(f.type));
    out = out.filter((f) => f.category == null || categoryFilter.accepts(f.category));
    out = out.filter((f) => severityFilter.accepts(f.severity));
    if (showStatus) {
      out = out.filter((f) => statusFilter.accepts(f.status));
    }
    if (minConfidence > 0) {
      out = out.filter((f) => f.confidence >= minConfidence / 100);
    }

    // Sort
    out = [...out];
    switch (sortBy) {
      case "severity":
        out.sort((a, b) => {
          const sd = (SEV_RANK[b.severity] ?? 0) - (SEV_RANK[a.severity] ?? 0);
          if (sd !== 0) return sd;
          return b.confidence - a.confidence;
        });
        break;
      case "newest":
        out.sort((a, b) => b.created_at - a.created_at);
        break;
      case "updated":
        // updated_at, not created_at: a finding moves when it is
        // triaged, fixed, or its PR changes state, so this surfaces what
        // the daemon and you have just been working on rather than what
        // happened to be found last.
        out.sort((a, b) => b.updated_at - a.updated_at);
        break;
      case "oldest":
        out.sort((a, b) => a.created_at - b.created_at);
        break;
    }

    return out;
  });

  // -- Push filtered output whenever it changes -----------------------------
  $effect(() => {
    onFilter(filtered);
  });
</script>

<div class="filter-bar">
  {#if needsSelector(repos, repoFilter)}
    <MultiSelect label="Repo" options={repos} filter={repoFilter} onChange={() => selectFilter("repo", repoFilter)} />
  {/if}

  {#if needsSelector(types, typeFilter)}
    <MultiSelect label="Type" options={types} filter={typeFilter} onChange={() => selectFilter("type", typeFilter)} />
  {/if}

  {#if needsSelector(categories, categoryFilter)}
    <MultiSelect label="Class" options={categories} filter={categoryFilter} onChange={() => selectFilter("class", categoryFilter)} />
  {/if}

  {#if needsSelector(severities, severityFilter)}
    <MultiSelect label="Severity" options={severities} filter={severityFilter} onChange={() => selectFilter("severity", severityFilter)} />
  {/if}

  {#if showStatus && needsSelector(statuses, statusFilter)}
    <MultiSelect label="Status" options={statuses} filter={statusFilter} onChange={() => selectFilter("status", statusFilter)} />
  {/if}

  <div class="control">
    <label for="{prefix}-conf" class="control-label">Conf ≥</label>
    <input
      id="{prefix}-conf"
      type="range"
      min="0" max="100" step="5"
      value={minConfidence}
      oninput={(event) => changeConfidence(Number(event.currentTarget.value))}
      onchange={() => { sliding = false; }}
      class="slider"
    />
    <span class="slider-value">{minConfidence}%</span>
  </div>

  <div class="control">
    <label for="{prefix}-sort" class="control-label">Sort</label>
    <select
      id="{prefix}-sort"
      value={sortBy}
      onchange={(event) => navigation.updateParams((params) => {
        const value = event.currentTarget.value;
        if (value === "severity") params.delete("sort");
        else params.set("sort", value);
      })}
      class="sort-select"
    >
      <option value="severity">Severity × Confidence</option>
      <option value="newest">Newest</option>
      <option value="updated">Recently updated</option>
      <option value="oldest">Oldest</option>
    </select>
  </div>
</div>

<style>
  .filter-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin-bottom: 1rem;
    font-size: 0.75rem;
  }


  /* ── Controls ────────────────────────────────────────────────── */
  .control {
    display: flex;
    align-items: center;
    gap: 0.375rem;
  }

  .control-label {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  /* Custom slider track + thumb */
  .slider {
    -webkit-appearance: none;
    appearance: none;
    width: 4.5rem;
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.1);
    outline: none;
  }
  /* `:focus-visible` rather than `:focus`: the ring exists for keyboard reach,
     so leave it to the UA to decide when a pointer drag doesn't warrant one. */
  .slider:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 3px;
  }
  .slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--accent);
    cursor: pointer;
    border: 2px solid var(--bg-panel);
    box-shadow: 0 0 0 1px rgba(78, 168, 222, 0.3);
  }
  .slider::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--accent);
    cursor: pointer;
    border: 2px solid var(--bg-panel);
  }
  .slider::-moz-range-track {
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.1);
  }

  .slider-value {
    color: var(--text-dim);
    width: 2rem;
    text-align: right;
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }

  .sort-select {
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: 99px;
    padding: 0.1875rem 0.5rem;
    color: var(--text);
    font-size: 0.6875rem;
  }
  .sort-select:focus {
    outline: none;
    border-color: var(--accent);
  }

  /* ── Responsive ──────────────────────────────────────────────── */
  @media (max-width: 600px) {
    .filter-bar {
      gap: 0.375rem;
    }
    .slider {
      width: 3rem;
    }
  }
</style>
