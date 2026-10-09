<script lang="ts">
  import { toasts } from "../lib/toast.svelte";

  // Some toasts report on a modal dialog that is still open (adding a
  // repo), and a modal's top layer paints over ordinary fixed-position
  // content, so the stack joins that layer to stay visible.
  let toastEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    if (toastEl && !toastEl.matches(":popover-open")) toastEl.showPopover();
  });
</script>

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

<style>
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
</style>
