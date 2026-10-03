<script lang="ts">
  import { store } from "../lib/api.svelte";

  let username = $state("");
  let password = $state("");
  let submitting = $state(false);
  let error = $state<string | null>(null);

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    submitting = true;
    error = null;
    try {
      error = await store.login(username, password);
    } finally {
      submitting = false;
    }
    // A failed attempt keeps the username; a stale password is never reused.
    if (error) password = "";
  }
</script>

<div class="login-wrap">
  <form class="login-card" onsubmit={submit}>
    <h2 class="login-title">hunter</h2>
    <label class="field-label">
      username
      <input
        class="field-input"
        type="text"
        name="username"
        autocomplete="username"
        required
        bind:value={username}
      />
    </label>
    <label class="field-label">
      password
      <input
        class="field-input"
        type="password"
        name="password"
        autocomplete="current-password"
        required
        bind:value={password}
      />
    </label>
    {#if error}
      <p class="login-error" role="alert">{error}</p>
    {/if}
    <button class="login-btn" type="submit" disabled={submitting}>
      {submitting ? "Signing in…" : "Sign in"}
    </button>
  </form>
</div>

<style>
  .login-wrap {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 1rem;
  }

  .login-card {
    display: flex;
    flex-direction: column;
    gap: 0.875rem;
    width: 100%;
    max-width: 20rem;
    background: var(--bg-card);
    padding: 1.5rem;
    border: 1px solid var(--border-hover);
    border-radius: var(--radius-lg);
    animation: fadeIn 150ms ease both;
  }

  .login-title {
    font-size: 0.9375rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--text);
    margin: 0 0 0.25rem;
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
  .field-input:focus {
    border-color: var(--accent);
    outline: none;
  }

  .login-error {
    margin: 0;
    font-size: 0.75rem;
    color: var(--bad);
  }

  .login-btn {
    font-size: 0.8125rem;
    background: rgba(78, 168, 222, 0.1);
    color: var(--accent);
    padding: 0.4375rem 0.75rem;
    border-radius: var(--radius-sm);
    font-weight: 600;
    border: 1px solid rgba(78, 168, 222, 0.25);
  }
  .login-btn:hover:not(:disabled) {
    background: rgba(78, 168, 222, 0.18);
  }
  .login-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
