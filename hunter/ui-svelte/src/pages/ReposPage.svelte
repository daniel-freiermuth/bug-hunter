<script lang="ts">
  import { store, post } from "../lib/api.svelte";
  import { isHttpUrl } from "../lib/format";
  import { datetime } from "../lib/format";
  import { SvelteMap, SvelteSet } from "svelte/reactivity";
  import { untrack } from "svelte";
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

  // Add-repo failures are reported while its modal dialog is still open, and a
  // modal's top layer paints over ordinary fixed-position content — so the
  // toast stack has to join that layer to stay visible.
  let toastEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    if (toastEl && !toastEl.matches(":popover-open")) toastEl.showPopover();
  });

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

  // Repos with a note write in flight. A second click would read the same
  // textarea contents and persist the note twice, and its reload could land
  // after the first one and redisplay the pre-write notes.
  const notesSaving = new SvelteSet<number>();

  let repos = $derived(store.summary?.repos ?? []);

  // A repo id identifies a repo only among the repos alive at the same
  // moment. `repos.id` is `INTEGER PRIMARY KEY` with no AUTOINCREMENT, so it
  // aliases the rowid and SQLite hands the highest freed one to the next
  // INSERT: delete the newest repo, add another, and it is issued the same
  // id. Every collection below is keyed by that id, and `repoNotesCache`
  // is never invalidated by anything — so the replacement repo would open
  // its notes panel showing the deleted repo's notes, with no request made
  // and nothing on screen saying the content is not its own.
  //
  // Invalidating on *identity* rather than on the id disappearing: the id
  // does not disappear in the case that matters. Delete and re-add between
  // two polls and the client only ever sees id 15 followed by id 15, so an
  // absence check never fires. It also covers the repo being replaced by
  // the daemon or another tab, which no local delete handler would see.
  function repoIdentity(r: Repo): string {
    return `${r.url}\u0000${r.added_at}`;
  }
  // SvelteMap for the same reason as the collections it mirrors: the lint
  // rule bans a plain Map here, and keeping them the same kind avoids one
  // of the seven being the odd one out.
  const notesIdentity = new SvelteMap<number, string>();

  $effect(() => {
    const live = new Map(repos.map((r) => [r.id, repoIdentity(r)]));
    // Untracked: this effect writes the same collections it would otherwise
    // depend on, and would then re-run on its own writes.
    untrack(() => {
      for (const [id, identity] of [...notesIdentity]) {
        if (live.get(id) === identity) continue;
        notesIdentity.delete(id);
        notesContent.delete(id);
        newNoteText.delete(id);
        newNoteCategory.delete(id);
        expandedNotes.delete(id);
        notesLoading.delete(id);
        notesSaving.delete(id);
        store.repoNotesCache.delete(id);
      }
    });
  });

  async function toggleRepo(repo: Repo) {
    const enabled = repo.enabled ? 0 : 1;
    try {
      const r = await post("/api/repo", { id: repo.id, enabled });
      if (r.ok) {
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
      if (r.ok) {
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

  /** Returns true only when the repo was actually created. */
  async function addRepo(): Promise<boolean> {
    if (!newName.trim() || !newUrl.trim()) {
      toast("Name and URL are required", false);
      return false;
    }
    const body: Record<string, unknown> = {
      name: newName.trim(),
      url: newUrl.trim(),
      branch: newBranch.trim() || "main",
    };
    if (newForge) body.forge = newForge;
    try {
      const r = await post("/api/repos", body);
      if (!r.ok) {
        toast("Failed to add repo", false);
        return false;
      }
      toast(`Added ${newName}`, true);
      newName = "";
      newUrl = "";
      newBranch = "main";
      newForge = "";
      store.refresh();
      return true;
    } catch (err) {
      console.error(`add repo ${newUrl.trim()} failed`, err);
      toast("Failed to add repo", false);
      return false;
    }
  }

  async function toggleNotes(repoId: number) {
    if (expandedNotes.has(repoId)) {
      expandedNotes.delete(repoId);
      return;
    }
    // Expand before awaiting, or the panel's loading state can never paint and
    // a second click during the flight starts a duplicate request whose
    // completion re-opens a panel the user has just closed.
    expandedNotes.add(repoId);
    if (notesContent.has(repoId) || notesLoading.has(repoId)) return;
    notesLoading.add(repoId);
    try {
      const notes = await store.fetchRepoNotes(repoId);
      // The effect above cannot catch a write that lands after it ran, so a
      // response for a repo replaced mid-flight has to drop itself.
      const current = repos.find((r) => r.id === repoId);
      if (!current) return;
      notesIdentity.set(repoId, repoIdentity(current));
      notesContent.set(repoId, notes);
    } catch (err) {
      console.error(`load notes for repo ${repoId} failed`, err);
      toast("Failed to load notes", false);
      // Nothing to show and no retry without another click: collapse again.
      expandedNotes.delete(repoId);
    } finally {
      notesLoading.delete(repoId);
    }
  }

  async function addNote(repoId: number) {
    if (notesSaving.has(repoId)) return;
    const text = newNoteText.get(repoId)?.trim();
    if (!text) {
      toast("Note text is required", false);
      return;
    }
    const category = newNoteCategory.get(repoId)?.trim() || null;
    notesSaving.add(repoId);
    try {
      try {
        const r = await post("/api/repo/notes", {
          id: repoId,
          note: text,
          ...(category ? { category } : {}),
        });
        if (!r.ok) {
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
        const notes = await store.fetchRepoNotes(repoId);
        const current = repos.find((r) => r.id === repoId);
        if (!current) return;
        notesIdentity.set(repoId, repoIdentity(current));
        notesContent.set(repoId, notes);
      } catch (err) {
        console.error(`reload notes for repo ${repoId} failed`, err);
        toast("Note saved, but reloading notes failed", false);
      }
    } finally {
      notesSaving.delete(repoId);
    }
  }
</script>

<!-- Toast container -->
{#if toasts.length > 0}
  <!-- One polite region for both outcomes: every toast is the result of an
       action the user just took, so nothing warrants interrupting them, and a
       separate assertive region would let a failure jump ahead of a success
       still on screen. aria-atomic is off because role="status" defaults it on,
       which would re-read the whole stack each time a toast joins it. -->
  <div
    class="toast-container"
    bind:this={toastEl}
    popover="manual"
    role="status"
    aria-live="polite"
    aria-atomic="false"
  >
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
          onclick={async () => { if (!(await addRepo())) return; const d = document.getElementById('addRepoDialog') as HTMLDialogElement; d?.close(); }}
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
                  {#if isHttpUrl(repo.url)}
                    <a href={repo.url} target="_blank" rel="noopener" class="repo-link">
                      {repo.url}
                    </a>
                  {:else}
                    <!-- Not a browser-navigable URL (ssh clone URL, or a
                         row predating write-side validation): show it, do
                         not make it clickable. -->
                    <span class="repo-link">{repo.url}</span>
                  {/if}
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
                        disabled={notesSaving.has(repo.id)}
                        class="submit-btn"
                      >
                        {notesSaving.has(repo.id) ? "Saving…" : "Add Note"}
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
    /* Overrides the UA popover box: full inset, border, padding, background. */
    inset: 1rem 1rem auto auto;
    margin: 0;
    border: 0;
    padding: 0;
    background: transparent;
    overflow: visible;
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
  .submit-btn:hover:not(:disabled) { background: var(--accent-hover); }
  .submit-btn:disabled { opacity: 0.4; cursor: default; }

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
