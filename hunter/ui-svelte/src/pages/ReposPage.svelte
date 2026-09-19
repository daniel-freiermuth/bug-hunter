<script lang="ts">
  import { store, post } from "../lib/api.svelte";
  import { datetime } from "../lib/format";
  import { SvelteMap, SvelteSet } from "svelte/reactivity";
  import type { Repo } from "../lib/types";

  // Toast state
  let toasts = $state<{ id: number; msg: string; ok: boolean }[]>([]);
  let toastId = 0;

  function toast(msg: string, ok: boolean) {
    const id = ++toastId;
    toasts = [...toasts, { id, msg, ok }];
    setTimeout(() => {
      toasts = toasts.filter((t) => t.id !== id);
    }, 3000);
  }

  // Add-repo form state
  let newName = $state("");
  let newUrl = $state("");
  let newBranch = $state("main");
  let newForge = $state("");

  // Notes panel: which repo id is expanded. These reactive collections are
  // mutated in place — replacing the instance drops fine-grained invalidation.
  const expandedNotes = new SvelteSet<number>();
  const notesContent = new SvelteMap<number, string>();
  const notesLoading = new SvelteSet<number>();

  // New note form per repo
  const newNoteText = new SvelteMap<number, string>();
  const newNoteCategory = new SvelteMap<number, string>();

  let repos = $derived(store.summary?.repos ?? []);

  async function toggleRepo(repo: Repo) {
    const enabled = repo.enabled ? 0 : 1;
    try {
      const r = await post("/api/repo", { id: repo.id, enabled });
      if (r.status === 200) {
        toast(`${repo.name} ${enabled ? "enabled" : "paused"}`, true);
        store.refresh();
      } else {
        toast(`Failed to toggle ${repo.name}`, false);
      }
    } catch (err) {
      console.error(`toggle repo ${repo.name} (id ${repo.id}) failed`, err);
      toast(`Failed to toggle ${repo.name}`, false);
    }
  }

  async function removeRepo(repo: Repo) {
    if (!confirm(`Remove repo "${repo.name}"? This cannot be undone.`)) return;
    try {
      const r = await post("/api/repo/delete", { id: repo.id });
      if (r.status === 200) {
        toast(`Removed ${repo.name}`, true);
        store.refresh();
      } else {
        toast(`Failed to remove ${repo.name}`, false);
      }
    } catch (err) {
      console.error(`remove repo ${repo.name} (id ${repo.id}) failed`, err);
      toast(`Failed to remove ${repo.name}`, false);
    }
  }

  async function addRepo() {
    if (!newName.trim() || !newUrl.trim()) {
      toast("Name and URL are required", false);
      return;
    }
    const body: Record<string, unknown> = {
      name: newName.trim(),
      url: newUrl.trim(),
      branch: newBranch.trim() || "main",
    };
    if (newForge) body.forge = newForge;
    try {
      const r = await post("/api/repos", body);
      if (r.status === 201) {
        toast(`Added ${newName}`, true);
        newName = "";
        newUrl = "";
        newBranch = "main";
        newForge = "";
        store.refresh();
      } else {
        toast("Failed to add repo", false);
      }
    } catch (err) {
      console.error(`add repo ${newUrl.trim()} failed`, err);
      toast("Failed to add repo", false);
    }
  }

  async function toggleNotes(repoId: number) {
    if (expandedNotes.has(repoId)) {
      expandedNotes.delete(repoId);
      return;
    }
    // Lazy-load notes
    if (!notesContent.has(repoId)) {
      notesLoading.add(repoId);
      try {
        notesContent.set(repoId, await store.fetchRepoNotes(repoId));
      } catch (err) {
        console.error(`load notes for repo ${repoId} failed`, err);
        toast("Failed to load notes", false);
        return;
      } finally {
        notesLoading.delete(repoId);
      }
    }
    expandedNotes.add(repoId);
  }

  async function addNote(repoId: number) {
    const text = newNoteText.get(repoId)?.trim();
    if (!text) {
      toast("Note text is required", false);
      return;
    }
    const category = newNoteCategory.get(repoId)?.trim() || null;
    try {
      const r = await post("/api/repo/notes", {
        id: repoId,
        note: text,
        ...(category ? { category } : {}),
      });
      if (r.status !== 201) {
        toast("Failed to add note", false);
        return;
      }
    } catch (err) {
      console.error(`add note to repo ${repoId} failed`, err);
      toast("Failed to add note", false);
      return;
    }
    toast("Note added", true);
    // Clear form
    newNoteText.delete(repoId);
    newNoteCategory.delete(repoId);
    // Invalidate cache and reload
    store.repoNotesCache.delete(repoId);
    try {
      notesContent.set(repoId, await store.fetchRepoNotes(repoId));
    } catch (err) {
      console.error(`reload notes for repo ${repoId} failed`, err);
      toast("Note saved, but reloading notes failed", false);
    }
  }
</script>

<!-- Toast container -->
{#if toasts.length > 0}
  <div class="toast-container">
    {#each toasts as t (t.id)}
      <div class="toast" class:ok={t.ok} class:fail={!t.ok}>
        {t.msg}
      </div>
    {/each}
  </div>
{/if}

<div class="page-enter">
  <div class="page">
    <h2 class="page-title">
      Repos
      <button
        onclick={() => { const d = document.getElementById('addRepoDialog') as HTMLDialogElement; d?.showModal(); }}
        class="add-btn"
      >
        + Add Repo
      </button>
    </h2>

    <!-- Add Repo dialog (modal) -->
    <dialog id="addRepoDialog" class="add-dialog">
      <h3 class="dialog-title">Add Repo</h3>
      <div class="form-fields">
        <label class="field-label">
          name
          <input type="text" placeholder="my-repo" bind:value={newName}
            class="field-input" />
        </label>
        <label class="field-label">
          url
          <input type="text" placeholder="https://github.com/org/repo" bind:value={newUrl}
            class="field-input" />
        </label>
        <label class="field-label">
          branch
          <input type="text" placeholder="main" bind:value={newBranch}
            class="field-input" />
        </label>
        <label class="field-label">
          forge
          <select bind:value={newForge}
            class="field-input">
            <option value="">auto-detect</option>
            <option value="github">github</option>
            <option value="gitlab">gitlab</option>
          </select>
        </label>
      </div>
      <div class="dialog-actions">
        <button
          onclick={() => { const d = document.getElementById('addRepoDialog') as HTMLDialogElement; d?.close(); }}
          class="cancel-btn"
        >
          Cancel
        </button>
        <button
          onclick={() => { addRepo(); const d = document.getElementById('addRepoDialog') as HTMLDialogElement; d?.close(); }}
          class="submit-btn"
        >
          Add
        </button>
      </div>
    </dialog>

    <!-- Repo list -->
    {#if repos.length === 0}
      <div class="empty-state">
        <div class="empty-icon">⌂</div>
        <p class="empty-title">No repositories</p>
        <p class="empty-sub">Add a repository to start hunting for bugs.</p>
      </div>
    {:else}
      <div class="repo-list">
        {#each repos as repo (repo.id)}
          <div class="repo-card">
            <!-- Repo header -->
            <div class="repo-header">
              <div class="repo-info">
                <div class="repo-name-row">
                  <span class="repo-name">{repo.name}</span>
                  <span class="forge-badge" class:github={repo.forge === 'github'} class:gitlab={repo.forge !== 'github'}>
                    {repo.forge}
                  </span>
                  {#if !repo.enabled}
                    <span class="paused-badge">paused</span>
                  {/if}
                </div>
                <div class="repo-meta">
                  <a href={repo.url} target="_blank" rel="noopener" class="repo-link">
                    {repo.url}
                  </a>
                  <span class="meta-sep">·</span>
                  <span class="branch">{repo.default_branch}</span>
                  <span class="meta-sep">·</span>
                  last hunt: {datetime(repo.last_hunt_at)}
                </div>
              </div>
              <div class="repo-actions">
                <button
                  onclick={() => toggleNotes(repo.id)}
                  class="notes-btn"
                >
                  Notes {expandedNotes.has(repo.id) ? '▾' : '▸'}
                </button>
                <button
                  onclick={() => toggleRepo(repo)}
                  class="toggle-btn"
                  class:is-pausing={repo.enabled}
                  class:is-resuming={!repo.enabled}
                >
                  {repo.enabled ? 'Pause' : 'Resume'}
                </button>
                <button
                  onclick={() => removeRepo(repo)}
                  class="remove-btn"
                >
                  Remove
                </button>
              </div>
            </div>

            <!-- Notes panel (collapsible) -->
            {#if expandedNotes.has(repo.id)}
              <div class="notes-panel">
                {#if notesLoading.has(repo.id)}
                  <p class="notes-empty">Loading notes…</p>
                {:else}
                  {@const notes = notesContent.get(repo.id) ?? ""}
                  {#if notes}
                    <pre class="notes-content">{notes}</pre>
                  {:else}
                    <p class="notes-empty">No notes yet.</p>
                  {/if}
                  <!-- Add note form -->
                  <div class="note-form">
                    <textarea
                      placeholder="Add a note…"
                      value={newNoteText.get(repo.id) ?? ""}
                      oninput={(e: Event) => {
                        newNoteText.set(repo.id, (e.target as HTMLTextAreaElement).value);
                      }}
                      class="note-input"
                    ></textarea>
                    <div class="note-actions">
                      <input
                        type="text"
                        placeholder="Category (optional)"
                        value={newNoteCategory.get(repo.id) ?? ""}
                        oninput={(e: Event) => {
                          newNoteCategory.set(repo.id, (e.target as HTMLInputElement).value);
                        }}
                        class="note-category"
                      />
                      <button
                        onclick={() => addNote(repo.id)}
                        class="submit-btn"
                      >
                        Add Note
                      </button>
                    </div>
                  </div>
                {/if}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>

<style>
  /* ── Toast ───────────────────────────────────────────────────── */
  .toast-container {
    position: fixed;
    top: 1rem;
    right: 1rem;
    z-index: 60;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .toast {
    padding: 0.5rem 1rem;
    border-radius: var(--radius-md);
    font-size: 0.8125rem;
    font-weight: 500;
    box-shadow: 0 8px 20px rgba(0, 0, 0, 0.3);
    animation: fadeIn 200ms ease both;
  }
  .toast.ok {
    background: rgba(68, 238, 136, 0.9);
    color: var(--bg);
  }
  .toast.fail {
    background: rgba(238, 85, 68, 0.9);
    color: #fff;
  }

  /* ── Page ────────────────────────────────────────────────────── */
  .page {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .add-btn {
    font-size: 0.75rem;
    background: rgba(78, 168, 222, 0.1);
    color: var(--accent);
    padding: 0.25rem 0.75rem;
    border-radius: 99px;
    font-weight: 500;
    border: 1px solid rgba(78, 168, 222, 0.15);
  }
  .add-btn:hover {
    background: rgba(78, 168, 222, 0.18);
    border-color: rgba(78, 168, 222, 0.25);
  }

  /* ── Dialog ──────────────────────────────────────────────────── */
  .add-dialog {
    background: var(--bg-card);
    color: var(--text);
    padding: 1.5rem;
    border: 1px solid var(--border-hover);
    border-radius: var(--radius-lg);
    max-width: 26rem;
    width: 100%;
    box-shadow: 0 20px 50px rgba(0, 0, 0, 0.5);
  }

  .dialog-title {
    font-size: 1rem;
    font-weight: 600;
    margin-bottom: 1rem;
  }

  .form-fields {
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }

  .field-label {
    display: block;
    font-size: 0.75rem;
    color: var(--text-dim);
    font-weight: 500;
  }

  .field-input {
    margin-top: 0.1875rem;
    width: 100%;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.375rem 0.625rem;
    font-size: 0.8125rem;
    color: var(--text);
    transition: border-color var(--transition);
  }
  .field-input::placeholder {
    color: rgba(136, 136, 136, 0.45);
  }
  .field-input:focus {
    border-color: var(--accent);
    outline: none;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 1.25rem;
  }

  .cancel-btn {
    padding: 0.375rem 0.875rem;
    border-radius: var(--radius-sm);
    font-size: 0.8125rem;
    color: var(--text-dim);
  }
  .cancel-btn:hover { color: var(--text); }

  .submit-btn {
    background: var(--accent);
    color: var(--bg);
    font-weight: 600;
    padding: 0.375rem 0.875rem;
    border-radius: var(--radius-sm);
    font-size: 0.8125rem;
  }
  .submit-btn:hover { background: var(--accent-hover); }

  /* ── Empty ───────────────────────────────────────────────────── */
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

  /* ── Repo list ───────────────────────────────────────────────── */
  .repo-list {
    display: flex;
    flex-direction: column;
    gap: 0.625rem;
  }

  .repo-card {
    background: var(--bg-card);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    overflow: hidden;
    transition: border-color var(--transition), box-shadow var(--transition);
  }
  .repo-card:hover {
    border-color: var(--border-hover);
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
  }

  .repo-header {
    padding: 0.75rem 1rem;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .repo-info {
    flex: 1;
    min-width: 0;
  }

  .repo-name-row {
    display: flex;
    align-items: center;
    gap: 0.4375rem;
  }

  .repo-name {
    font-weight: 600;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .forge-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-weight: 500;
  }
  .forge-badge.github {
    background: rgba(88, 166, 255, 0.12);
    color: #79c0ff;
  }
  .forge-badge.gitlab {
    background: rgba(249, 115, 22, 0.12);
    color: #fdba74;
  }

  .paused-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    background: rgba(136, 136, 136, 0.12);
    color: var(--stale);
  }

  .repo-meta {
    font-size: 0.6875rem;
    color: var(--text-dim);
    margin-top: 0.1875rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .repo-link:hover { color: var(--accent-hover); }

  .meta-sep { margin: 0 0.25rem; opacity: 0.4; }

  .branch {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }

  .repo-actions {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    flex-shrink: 0;
  }

  .notes-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.5rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    color: var(--text-dim);
  }
  .notes-btn:hover {
    color: var(--text);
    border-color: var(--border-hover);
  }

  .toggle-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.625rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
  }
  .toggle-btn.is-pausing {
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .toggle-btn.is-pausing:hover {
    background: rgba(136, 136, 136, 0.15);
  }
  .toggle-btn.is-resuming {
    color: var(--ok);
    background: rgba(68, 238, 136, 0.08);
  }
  .toggle-btn.is-resuming:hover {
    background: rgba(68, 238, 136, 0.18);
  }

  .remove-btn {
    font-size: 0.6875rem;
    padding: 0.1875rem 0.625rem;
    border-radius: var(--radius-sm);
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .remove-btn:hover {
    background: rgba(238, 85, 68, 0.15);
    color: var(--bad);
  }

  /* ── Notes panel ─────────────────────────────────────────────── */
  .notes-panel {
    border-top: 1px solid var(--border);
    background: var(--bg-panel);
    padding: 0.75rem 1rem;
    animation: fadeIn 150ms ease both;
  }

  .notes-content {
    font-size: 0.75rem;
    color: var(--text-dim);
    white-space: pre-wrap;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    margin-bottom: 0.75rem;
    max-height: 12rem;
    overflow: auto;
    background: var(--bg);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.625rem;
    border: 1px solid var(--border);
    line-height: 1.5;
  }

  .notes-empty {
    color: var(--text-dim);
    font-size: 0.75rem;
    margin-bottom: 0.75rem;
  }

  .note-form {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .note-input {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.375rem 0.625rem;
    font-size: 0.8125rem;
    color: var(--text);
    resize: vertical;
    min-height: 3.5rem;
    line-height: 1.5;
    transition: border-color var(--transition);
  }
  .note-input::placeholder { color: rgba(136, 136, 136, 0.45); }
  .note-input:focus { border-color: var(--accent); outline: none; }

  .note-actions {
    display: flex;
    gap: 0.375rem;
    align-items: center;
  }

  .note-category {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 0.25rem 0.625rem;
    font-size: 0.8125rem;
    color: var(--text);
    flex: 1;
    max-width: 12rem;
    transition: border-color var(--transition);
  }
  .note-category::placeholder { color: rgba(136, 136, 136, 0.45); }
  .note-category:focus { border-color: var(--accent); outline: none; }

  /* ── Responsive ──────────────────────────────────────────────── */
  @media (max-width: 600px) {
    .repo-header {
      flex-direction: column;
      align-items: flex-start;
      gap: 0.5rem;
    }
    .repo-actions {
      width: 100%;
    }
  }
</style>
