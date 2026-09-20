<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { store } from "./lib/api.svelte";
  import StatusPage from "./pages/StatusPage.svelte";
  import InboxPage from "./pages/InboxPage.svelte";
  import KanbanPage from "./pages/KanbanPage.svelte";
  import AllFindingsPage from "./pages/AllFindingsPage.svelte";
  import ReposPage from "./pages/ReposPage.svelte";
  import StatsPage from "./pages/StatsPage.svelte";
  import LogPage from "./pages/LogPage.svelte";
  import "./app.css";

  const NAV = [
    { id: "status", label: "STATUS", icon: "◈" },
    { id: "inbox", label: "INBOX", icon: "▣" },
    { id: "kanban", label: "KANBAN", icon: "▥" },
    { id: "findings", label: "FINDINGS", icon: "◉" },
    { id: "repos", label: "REPOS", icon: "⌂" },
    { id: "stats", label: "STATS", icon: "▤" },
    { id: "log", label: "LOG", icon: "≡" },
  ] as const;

  function parseHash(hash: string): { page: string; focusId: number | null } {
    const raw = hash.slice(1) || "inbox";
    const colon = raw.indexOf(":");
    if (colon >= 0) {
      const id = parseInt(raw.slice(colon + 1), 10);
      return { page: raw.slice(0, colon), focusId: Number.isNaN(id) ? null : id };
    }
    return { page: raw, focusId: null };
  }

  const initial = parseHash(location.hash);
  let page = $state(initial.page);
  let focusId = $state<number | null>(initial.focusId);
  let sidebarOpen = $state(false);

  function navigate(id: string) {
    location.hash = id;
    const h = parseHash(`#${id}`);
    page = h.page;
    focusId = h.focusId;
    sidebarOpen = false;
  }

  function onHashChange() {
    const h = parseHash(location.hash);
    page = h.page;
    focusId = h.focusId;
  }

  onMount(() => {
    store.startPolling();
    window.addEventListener("hashchange", onHashChange);
  });

  onDestroy(() => {
    store.stopPolling();
    window.removeEventListener("hashchange", onHashChange);
  });
</script>

<!-- Mobile top bar -->
<header class="topbar">
  <button
    class="hamburger"
    onclick={() => { sidebarOpen = !sidebarOpen; }}
    aria-label="Toggle navigation"
    aria-expanded={sidebarOpen}
    aria-controls="sidebar"
  >
    <svg width="18" height="18" viewBox="0 0 18 18" fill="none">
      <line x1="2" y1="4" x2="16" y2="4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
      <line x1="2" y1="9" x2="16" y2="9" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
      <line x1="2" y1="14" x2="16" y2="14" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
    </svg>
  </button>
  <svg viewBox="0 0 40 40" width="20" height="20" fill="none" xmlns="http://www.w3.org/2000/svg">
    <circle cx="18" cy="18" r="11" stroke="#5aa0e0" stroke-width="2.2" opacity=".85"/>
    <ellipse cx="18" cy="19" rx="5" ry="6.5" fill="#4cbf6b" opacity=".9"/>
    <circle cx="18" cy="11.5" r="2.8" fill="#4cbf6b" opacity=".9"/>
  </svg>
  <span class="topbar-brand">hunter</span>
</header>

<!-- Sidebar overlay backdrop (mobile) -->
{#if sidebarOpen}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="sidebar-backdrop" onclick={() => { sidebarOpen = false; }}></div>
{/if}

<div class="shell">
  <!-- Sidebar nav -->
  <nav class="sidebar" class:open={sidebarOpen} id="sidebar">
    <div class="logo">
      <svg viewBox="0 0 40 40" width="28" height="28" fill="none" xmlns="http://www.w3.org/2000/svg">
        <!-- magnifying glass -->
        <circle cx="18" cy="18" r="11" stroke="#5aa0e0" stroke-width="2.2" opacity=".85"/>
        <line x1="26" y1="26" x2="36" y2="36" stroke="#5aa0e0" stroke-width="2.5" stroke-linecap="round" opacity=".85"/>
        <!-- bug body -->
        <ellipse cx="18" cy="19" rx="5" ry="6.5" fill="#4cbf6b" opacity=".9"/>
        <!-- head -->
        <circle cx="18" cy="11.5" r="2.8" fill="#4cbf6b" opacity=".9"/>
        <!-- legs -->
        <line x1="13" y1="15" x2="9"  y2="12" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="13" y1="19" x2="8"  y2="19" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="13" y1="23" x2="9"  y2="26" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="15" x2="27" y2="12" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="19" x2="28" y2="19" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <line x1="23" y1="23" x2="27" y2="26" stroke="#4cbf6b" stroke-width="1.4" stroke-linecap="round"/>
        <!-- wing line -->
        <line x1="18" y1="13" x2="18" y2="25" stroke="var(--bg-panel)" stroke-width="1" opacity=".6"/>
        <!-- antennae -->
        <line x1="16" y1="9.5" x2="13" y2="6" stroke="#4cbf6b" stroke-width="1.2" stroke-linecap="round"/>
        <line x1="20" y1="9.5" x2="23" y2="6" stroke="#4cbf6b" stroke-width="1.2" stroke-linecap="round"/>
        <circle cx="13" cy="5.5" r=".9" fill="#4cbf6b"/>
        <circle cx="23" cy="5.5" r=".9" fill="#4cbf6b"/>
      </svg>
      <span class="brand">hunter</span>
    </div>
    <div class="nav-items">
      {#each NAV as { id, label, icon } (id)}
        <button
          class="nav-btn"
          class:active={page === id}
          onclick={() => navigate(id)}
        >
          <span class="nav-icon">{icon}</span>
          {label}
        </button>
      {/each}
    </div>
  </nav>

  <!-- Main content -->
  <main class="content">
    {#if store.error}
      <div class="api-banner" role="status">
        <span class="api-banner-icon">⚠</span>
        <span>Cannot reach the API ({store.error}) — data shown below may be stale.</span>
      </div>
    {/if}
    {#key page}
      <div class="page-enter">
        {#if page === "status"}
          <StatusPage />
        {:else if page === "inbox"}
          <InboxPage />
        {:else if page === "kanban"}
          <KanbanPage />
        {:else if page === "findings"}
          <AllFindingsPage {focusId} />
        {:else if page === "repos"}
          <ReposPage />
        {:else if page === "stats"}
          <StatsPage />
        {:else if page === "log"}
          <LogPage />
        {:else}
          <InboxPage />
        {/if}
      </div>
    {/key}
  </main>
</div>

<style>
  /* ── Mobile top bar ──────────────────────────────────────────── */
  .topbar {
    display: none;
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    height: 48px;
    background: var(--bg-panel);
    border-bottom: 1px solid var(--border);
    align-items: center;
    padding: 0 0.875rem;
    gap: 0.625rem;
    z-index: 40;
  }

  .hamburger {
    color: var(--text-dim);
    padding: 0.375rem;
    border-radius: var(--radius-sm);
    display: flex;
    align-items: center;
  }
  .hamburger:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.06);
  }

  .topbar-brand {
    font-size: 0.8125rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--text);
  }

  /* ── Sidebar backdrop (mobile) ──────────────────────────────── */
  .sidebar-backdrop {
    display: none;
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    backdrop-filter: blur(2px);
    z-index: 49;
  }

  /* ── Shell ───────────────────────────────────────────────────── */
  .shell {
    display: flex;
    height: 100vh;
  }

  /* ── Sidebar ─────────────────────────────────────────────────── */
  .sidebar {
    width: 11.5rem;
    flex-shrink: 0;
    background: var(--bg-panel);
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border);
    z-index: 50;
  }

  .logo {
    display: flex;
    align-items: center;
    gap: 0.625rem;
    padding: 0.875rem 1.25rem;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
    position: relative;
    z-index: 1;
  }

  .brand {
    font-size: 0.8125rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--text);
    text-transform: lowercase;
  }

  .nav-items {
    display: flex;
    flex-direction: column;
    padding: 0.375rem 0;
    flex: 1;
    overflow-y: auto;
  }

  .nav-btn {
    padding: 0.5rem 0.875rem;
    text-align: left;
    font-size: 0.6875rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    color: var(--text-dim);
    background: none;
    border: none;
    border-left: 2px solid transparent;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .nav-icon {
    font-size: 0.6875rem;
    width: 1rem;
    text-align: center;
    opacity: 0.5;
  }

  .nav-btn:hover {
    color: var(--text);
    background: rgba(255, 255, 255, 0.035);
    border-left-color: rgba(255, 255, 255, 0.12);
  }

  .nav-btn.active {
    color: var(--accent);
    font-weight: 600;
    background: rgba(78, 168, 222, 0.07);
    border-left-color: var(--accent);
  }
  .nav-btn.active .nav-icon {
    opacity: 1;
  }

  /* ── Main content ────────────────────────────────────────────── */
  .content {
    flex: 1;
    overflow: auto;
    padding: 1.5rem 2rem;
    scroll-behavior: smooth;
  }

  .page-enter {
    animation: fadeIn 150ms ease both;
  }

  .api-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 1.25rem;
    padding: 0.5rem 0.75rem;
    background: var(--bg-elevated);
    border: 1px solid var(--bad);
    border-radius: var(--radius-sm);
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .api-banner-icon {
    color: var(--bad);
  }

  /* ── Mobile (< 768px) ────────────────────────────────────────── */
  @media (max-width: 767px) {
    .topbar {
      display: flex;
    }

    .sidebar-backdrop {
      display: block;
    }

    .shell {
      padding-top: 48px;
    }

    .sidebar {
      position: fixed;
      top: 0;
      left: 0;
      bottom: 0;
      transform: translateX(-100%);
      /* visibility (not just transform) is what drops the closed drawer out of
         the tab order and the accessibility tree; transitioning it keeps the
         panel rendered for the full slide-out before it flips to hidden. */
      visibility: hidden;
      transition:
        transform 200ms cubic-bezier(0.16, 1, 0.3, 1),
        visibility 200ms;
      box-shadow: 4px 0 20px rgba(0, 0, 0, 0.4);
    }

    .sidebar.open {
      transform: translateX(0);
      visibility: visible;
    }

    .content {
      padding: 1rem;
    }
  }
</style>
