/** One toast stack for the whole app, rendered once by Toasts.svelte. */

export type Toast = { id: number; msg: string; ok: boolean };

const TOAST_MS = 3000;
let nextId = 0;

export const toasts = $state<Toast[]>([]);

/** Show `msg` for a few seconds; `ok` picks the success or failure style. */
export function toast(msg: string, ok: boolean): void {
  const id = ++nextId;
  toasts.push({ id, msg, ok });
  setTimeout(() => {
    const index = toasts.findIndex((t) => t.id === id);
    if (index >= 0) toasts.splice(index, 1);
  }, TOAST_MS);
}
