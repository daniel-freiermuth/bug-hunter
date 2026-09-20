<script lang="ts">
  import { SvelteSet } from "svelte/reactivity";
  import * as sel from "../lib/selection";

  let {
    label,
    options,
    selected,
  }: {
    label: string;
    options: string[];
    selected: SvelteSet<string>;
  } = $props();

  let open = $state(false);
  let container: HTMLDivElement | undefined = $state();

  // Membership, not size -- see lib/selection.ts for why `selected` is not
  // a subset of `options`. Pinned by lib/selection.test.ts.
  const allSelected = $derived(sel.allSelected(options, selected));
  const noneSelected = $derived(sel.noneSelected(options, selected));
  const filtering = $derived(!allSelected);

  // Trigger label: show what's selected when filtering.
  const triggerLabel = $derived.by(() => {
    if (!filtering) return label;
    if (noneSelected) return `${label}: none`;
    const names = options.filter(o => selected.has(o));
    const joined = names.join(", ");
    if (names.length <= 3 && joined.length <= 30) return joined;
    return `${label} (${names.length})`;
  });

  // Mutate the set in place, never reassign it: the SvelteSet instance is
  // itself the reactive value, so replacing it costs the parent its
  // subscription and forces a redundant `$state` wrapper on every caller.
  function toggle(value: string) {
    if (selected.has(value)) selected.delete(value);
    else selected.add(value);
  }

  function toggleAll() {
    if (allSelected) {
      // All checked → clear everything.
      selected.clear();
    } else {
      // Unchecked or indeterminate → select all.
      for (const option of options) selected.add(option);
    }
  }

  function handleClickOutside(e: MouseEvent) {
    if (container && !container.contains(e.target as Node)) {
      open = false;
    }
  }

  $effect(() => {
    if (open) {
      document.addEventListener("click", handleClickOutside, true);
      return () => document.removeEventListener("click", handleClickOutside, true);
    }
  });
</script>

<div class="multi-select" bind:this={container}>
  <button
    class="trigger"
    class:has-filter={filtering}
    onclick={() => { open = !open; }}
    aria-expanded={open}
    type="button"
  >
    {triggerLabel}
    <span class="arrow">{open ? "▴" : "▾"}</span>
  </button>

  {#if open}
    <!-- Not `listbox`: these are native checkboxes, which carry their own
         semantics and no listbox keyboard model. -->
    <div class="panel" role="group" aria-label={label}>
      <label class="option all-option">
        <input
          type="checkbox"
          checked={allSelected}
          indeterminate={!allSelected && !noneSelected}
          onchange={toggleAll}
        />
        All
      </label>
      <hr class="divider" />
      {#each options as opt (opt)}
        <label class="option">
          <input
            type="checkbox"
            checked={selected.has(opt)}
            onchange={() => toggle(opt)}
          />
          {opt}
        </label>
      {/each}
    </div>
  {/if}
</div>

<style>
  .multi-select {
    position: relative;
  }

  .trigger {
    padding: 0.25rem 0.5rem;
    border-radius: 99px;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    color: var(--text-dim);
    cursor: pointer;
    font-size: 0.6875rem;
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    user-select: none;
  }
  .trigger:hover {
    border-color: var(--border-hover);
    color: var(--text);
  }
  .trigger.has-filter {
    border-color: var(--accent);
    color: var(--text);
  }

  .badge {
    background: var(--accent);
    color: var(--bg);
    font-size: 0.5625rem;
    font-weight: 700;
    padding: 0 0.3rem;
    border-radius: 99px;
    min-width: 1rem;
    text-align: center;
    line-height: 1.1rem;
  }

  .arrow {
    font-size: 0.5rem;
    opacity: 0.5;
  }

  .panel {
    position: absolute;
    z-index: 20;
    top: calc(100% + 2px);
    left: 0;
    background: rgba(15, 22, 40, 0.95);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    border: 1px solid var(--border-hover);
    border-radius: var(--radius-md);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4), 0 2px 8px rgba(0, 0, 0, 0.2);
    padding: 0.375rem;
    min-width: 140px;
    max-height: 16rem;
    overflow-y: auto;
  }

  .option {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    padding: 0.25rem 0.375rem;
    cursor: pointer;
    color: var(--text-dim);
    border-radius: var(--radius-sm);
    font-size: 0.6875rem;
    user-select: none;
  }
  .option:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.04);
  }

  .all-option {
    font-weight: 600;
    color: var(--text);
  }

  .divider {
    border: none;
    border-top: 1px solid var(--border);
    margin: 0.25rem 0;
  }

  input[type="checkbox"] {
    accent-color: var(--accent);
    width: 0.8rem;
    height: 0.8rem;
    cursor: pointer;
  }
</style>
