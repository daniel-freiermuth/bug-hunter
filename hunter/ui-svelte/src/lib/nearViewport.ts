// Tells a list item when it comes near the visible part of its scroll
// container, so long lists mount only what is on or near screen.

import type { Attachment } from "svelte/attachments";

/** How far beyond the visible area an item already counts as near. */
const MARGIN = "1000px 0px";

type Listener = (entry: IntersectionObserverEntry) => void;

// One observer per scroll container, shared by every item in it.
const observers = new Map<Element | null, { io: IntersectionObserver; count: number }>();
const listeners = new WeakMap<Element, Listener>();
// Siblings share a scroll container; resolving it once per list keeps
// mounting a thousand items from walking the ancestors a thousand times.
const rootByParent = new WeakMap<Element, Element | null>();

/**
 * The element that scrolls `el` vertically, or null for the viewport.
 *
 * The observer has to use it as its root: `rootMargin` widens only the
 * root's box, while every scroll container in between clips at its own
 * edge. Against the viewport, an item inside a scrolling `<main>` would get
 * no margin at all and render blank for a frame as it scrolled in.
 * Overflowing is part of the test because `overflow-x: auto` computes
 * `overflow-y` to `auto` as well (Kanban's swipeable board on mobile),
 * without that element ever scrolling vertically.
 */
function scrollRoot(el: Element): Element | null {
  const parent = el.parentElement;
  if (!parent) return null;
  const cached = rootByParent.get(parent);
  if (cached !== undefined) return cached;
  let root: Element | null = null;
  for (let p: Element | null = parent; p; p = p.parentElement) {
    const { overflowY } = getComputedStyle(p);
    if ((overflowY === "auto" || overflowY === "scroll") && p.scrollHeight > p.clientHeight) {
      root = p;
      break;
    }
  }
  rootByParent.set(parent, root);
  return root;
}

/**
 * Calls `onChange` with each intersection change of the element against
 * its scroll container widened by MARGIN, starting with its initial state.
 */
export function nearViewport(onChange: Listener): Attachment<Element> {
  return (el) => {
    const root = scrollRoot(el);
    let entry = observers.get(root);
    if (!entry) {
      const io = new IntersectionObserver(
        (changes) => {
          for (const c of changes) listeners.get(c.target)?.(c);
        },
        { root, rootMargin: MARGIN },
      );
      entry = { io, count: 0 };
      observers.set(root, entry);
    }
    entry.count++;
    listeners.set(el, onChange);
    entry.io.observe(el);
    const shared = entry;
    return () => {
      shared.io.unobserve(el);
      listeners.delete(el);
      if (--shared.count === 0) {
        shared.io.disconnect();
        observers.delete(root);
      }
    };
  };
}
