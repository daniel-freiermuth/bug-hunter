<script lang="ts">
  import type { Finding } from "../lib/types";
  import { SEV_RANK } from "../lib/format";
  import { SvelteSet } from "svelte/reactivity";
  import { absorb, needsSelector, reselect } from "../lib/selection";
  import MultiSelect from "./MultiSelect.svelte";

  let {
    findings,
    prefix,
    showStatus = false,
    repoNames = new Map<number, string>(),
    onFilter,
  }: {
    findings: Finding[];
    prefix: string;
    showStatus?: boolean;
    repoNames?: Map<number, string>;
    onFilter: (filtered: Finding[]) => void;
  } = $props();

  // -- Extract unique values from the findings list -------------------------
  const repos = $derived([...new Set(findings.map((f) => repoNames.get(f.repo_id) ?? "unknown"))].sort());
  const types = $derived([...new Set(findings.map((f) => f.type))].sort());
  const categories = $derived([...new Set(findings.filter((f) => f.category).map((f) => f.category!))].sort());
  const severities = $derived([...new Set(findings.map((f) => f.severity))].sort((a, b) => (SEV_RANK[b] ?? 0) - (SEV_RANK[a] ?? 0)));
  const statuses = $derived([...new Set(findings.map((f) => f.status))].sort());

  // -- Filter selections (Svelte 5 runes) -----------------------------------
  // `const`, mutated in place: a SvelteSet is itself the reactive value, so
  // it needs no `$state` wrapper and must never be reassigned.
  const selectedRepos = new SvelteSet<string>();
  const selectedTypes = new SvelteSet<string>();
  const selectedCategories = new SvelteSet<string>();
  const selectedSeverities = new SvelteSet<string>();
  const selectedStatuses = new SvelteSet<string>();
  let minConfidence = $state(0);
  let sortBy = $state("severity");

  // Options this component has ever seen, per dimension. An option showing
  // up for the first time is selected automatically so new data cannot
  // silently hide itself; an option the user deselected is already known,
  // so it is never re-added behind their back. (A repo label changing from
  // "unknown" to its real name is simply a new option, and is absorbed.)
  const known = {
    repos: new Set<string>(),
    types: new Set<string>(),
    categories: new Set<string>(),
    severities: new Set<string>(),
    statuses: new Set<string>(),
  };

  // absorb / reselect / needsSelector live in lib/selection.ts, shared
  // with MultiSelect and pinned by lib/selection.test.ts.

  $effect(() => {
    absorb(repos, known.repos, selectedRepos);
    absorb(types, known.types, selectedTypes);
    absorb(categories, known.categories, selectedCategories);
    absorb(severities, known.severities, selectedSeverities);
    if (showStatus) absorb(statuses, known.statuses, selectedStatuses);
  });

  export function clearFilters() {
    reselect(selectedRepos, repos);
    reselect(selectedTypes, types);
    reselect(selectedCategories, categories);
    reselect(selectedSeverities, severities);
    if (showStatus) reselect(selectedStatuses, statuses);
    minConfidence = 0;
  }

  // -- Derived filtered + sorted output -------------------------------------
  const filtered = $derived.by(() => {
    let out = findings;

    out = out.filter((f) => selectedRepos.has(repoNames.get(f.repo_id) ?? "unknown"));
    out = out.filter((f) => selectedTypes.has(f.type));
    out = out.filter((f) => f.category == null || selectedCategories.has(f.category));
    out = out.filter((f) => selectedSeverities.has(f.severity));
    if (showStatus) {
      out = out.filter((f) => selectedStatuses.has(f.status));
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
      case "oldest":
        out.sort((a, b) => a.created_at - b.created_at);
        break;
      case "repo":
        out.sort((a, b) => {
          const ra = repoNames.get(a.repo_id) ?? "unknown";
          const rb = repoNames.get(b.repo_id) ?? "unknown";
          const rc = ra.localeCompare(rb);
          if (rc !== 0) return rc;
          return (SEV_RANK[b.severity] ?? 0) - (SEV_RANK[a.severity] ?? 0);
        });
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
  {#if needsSelector(repos, selectedRepos)}
    <MultiSelect label="Repo" options={repos} selected={selectedRepos} />
  {/if}

  {#if needsSelector(types, selectedTypes)}
    <MultiSelect label="Type" options={types} selected={selectedTypes} />
  {/if}

  {#if needsSelector(categories, selectedCategories)}
    <MultiSelect label="Class" options={categories} selected={selectedCategories} />
  {/if}

  {#if needsSelector(severities, selectedSeverities)}
    <MultiSelect label="Severity" options={severities} selected={selectedSeverities} />
  {/if}

  {#if showStatus && needsSelector(statuses, selectedStatuses)}
    <MultiSelect label="Status" options={statuses} selected={selectedStatuses} />
  {/if}

  <div class="control">
    <label for="{prefix}-conf" class="control-label">Conf ≥</label>
    <input
      id="{prefix}-conf"
      type="range"
      min="0" max="100" step="5"
      bind:value={minConfidence}
      class="slider"
    />
    <span class="slider-value">{minConfidence}%</span>
  </div>

  <div class="control">
    <label for="{prefix}-sort" class="control-label">Sort</label>
    <select
      id="{prefix}-sort"
      bind:value={sortBy}
      class="sort-select"
    >
      <option value="severity">Severity × Confidence</option>
      <option value="newest">Newest</option>
      <option value="oldest">Oldest</option>
      <option value="repo">By repo</option>
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
    width: 4.5rem;
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.1);
    outline: none;
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
