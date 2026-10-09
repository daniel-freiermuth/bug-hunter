// Where a page was scrolled, saved on its browser history entry so Back,
// Forward and reload come back to it. The Navigation API carries one state
// value per entry, survives reloads, and starts every new entry empty.

import { placeAtTop, settle } from "./scroll";

/**
 * A long list remounts with its off-screen cards at an estimated height,
 * so a pixel offset would land on a different card. Lists record the card
 * at the top of the view instead: which list (a page may have several,
 * like Kanban's columns, told apart by the attribute's value, since an
 * emptied column drops out of the page), the card's index, how far into it
 * the view had scrolled, and how far it was from the list's first card.
 */
type ListAnchor = { list: string; index: number; offset: number; span: number };

/**
 * `x`: horizontal offsets of the page's sideways scrollers (Kanban's
 * swipeable board on mobile), by name.
 */
export type ScrollState = { top: number; anchor: ListAnchor | null; x: Record<string, number> };

/**
 * Marks an element whose children are the cards of a scroll-restored
 * list; the value names the list among others on the same page.
 */
const SCROLL_LIST_ATTR = "data-scroll-list";

/** Marks an element that scrolls sideways; the value names it on the page. */
const SCROLL_X_ATTR = "data-scroll-x";

function listsIn(scroller: Element): HTMLElement[] {
  return [...scroller.querySelectorAll<HTMLElement>(`[${SCROLL_LIST_ATTR}]`)];
}

export function capture(scroller: Element): ScrollState {
  const top = scroller.scrollTop;
  const x: Record<string, number> = {};
  for (const el of scroller.querySelectorAll(`[${SCROLL_X_ATTR}]`)) {
    x[el.getAttribute(SCROLL_X_ATTR) ?? ""] = el.scrollLeft;
  }
  const view = scroller.getBoundingClientRect();
  // The first list, in page order, with a card in view anchors the page.
  // In view sideways too: on mobile, Kanban's columns sit side by side in
  // a board that scrolls horizontally and shows a sliver of the neighbours,
  // so a list counts when its middle is inside the board.
  for (const element of listsIn(scroller)) {
    const first = element.firstElementChild;
    const box = element.getBoundingClientRect();
    const clip = element.closest(`[${SCROLL_X_ATTR}]`)?.getBoundingClientRect() ?? view;
    const middle = (box.left + box.right) / 2;
    if (middle < clip.left || middle > clip.right) continue;
    let index = 0;
    for (const card of element.children) {
      const rect = card.getBoundingClientRect();
      if (rect.bottom > view.top) {
        if (first && rect.top < view.bottom) {
          const span = rect.top - first.getBoundingClientRect().top;
          const list = element.getAttribute(SCROLL_LIST_ATTR) ?? "";
          return { top, x, anchor: { list, index, offset: view.top - rect.top, span } };
        }
        break;
      }
      index++;
    }
  }
  return { top, x, anchor: null };
}

/** Space between consecutive cards of `list`: its gap plus a card's margins. */
function spacing(list: Element, card: Element): number {
  const cardStyle = getComputedStyle(card);
  return (
    (parseFloat(getComputedStyle(list).rowGap) || 0) +
    (parseFloat(cardStyle.marginTop) || 0) +
    (parseFloat(cardStyle.marginBottom) || 0)
  );
}

/**
 * Bring `scroller` back to `state`, or report that its content has not
 * rendered far enough yet. A list sizes its unrendered cards from the
 * recorded span first, so the scrollbar reads as it did, then puts the
 * recorded card back at the top; if the list has shrunk, the last card
 * stands in for it. Success means everything sits where it was: before the
 * data arrives (`ready` false) the recorded list may not exist yet and the
 * sideways scrollers may be too narrow, and while cards are still
 * placeholders the page may be too short to reach the card. Once the data
 * is there, a list or scroller still missing is gone (a Kanban column that
 * emptied meanwhile), and the saved pixel offset stands in for it.
 */
function apply(scroller: Element, state: ScrollState, ready: boolean): boolean {
  let sideways = true;
  for (const [name, left] of Object.entries(state.x)) {
    const el = scroller.querySelector(`[${SCROLL_X_ATTR}="${CSS.escape(name)}"]`);
    if (!el) {
      sideways &&= ready;
      continue;
    }
    el.scrollTo({ left, behavior: "instant" });
    if (Math.abs(el.scrollLeft - left) >= 1) sideways = false;
  }
  const key = state.anchor?.list;
  const list = state.anchor && listsIn(scroller).find((l) => (l.getAttribute(SCROLL_LIST_ATTR) ?? "") === key);
  if (state.anchor && list) {
    const cards = list.children;
    const index = Math.min(state.anchor.index, cards.length - 1);
    const card = cards[index];
    if (!card) return false; // the list has not rendered yet
    if (index > 0) {
      const estimate = (state.anchor.span - index * spacing(list, card)) / index;
      if (estimate > 0) list.style.setProperty("--card-estimate", `${estimate}px`);
    }
    return placeAtTop(card, -state.anchor.offset) && sideways;
  }
  if (state.anchor && !ready) return false; // its list has not rendered yet
  // A page shorter than the saved offset (still rendering, or with less
  // content than when it was saved) gets as close as it can now: the
  // browser clamps to its end. Waiting goes on in case it grows.
  scroller.scrollTo({ top: state.top, behavior: "instant" });
  return Math.abs(scroller.scrollTop - state.top) < 1 && sideways;
}

/**
 * Restore `state` as soon as the page has rendered enough to hold it.
 * `ready` says whether the page's data has arrived. Returns a cancel
 * function.
 */
export function restore(scroller: HTMLElement, state: ScrollState, ready: () => boolean): () => void {
  const content = scroller.firstElementChild;
  if (!content) return () => {};
  return settle(content, () => apply(scroller, state, ready()));
}

// The state names the entry it was saved on: following a plain `#…` link
// copies the current entry's state into the new entry, which would make a
// fresh navigation look like a return.
type EntryState = { entry: string; scroll: ScrollState };

/** The scroll state saved on the current history entry, if any. */
export function saved(): ScrollState | null {
  const current = window.navigation.currentEntry;
  const state = current?.getState() as EntryState | undefined;
  return state && state.entry === current?.key ? state.scroll : null;
}

/** Save `scroller`'s position on the current history entry. */
export function save(scroller: Element): void {
  const entry = window.navigation.currentEntry?.key;
  if (entry === undefined) return;
  const state: EntryState = { entry, scroll: capture(scroller) };
  window.navigation.updateCurrentEntry({ state });
}
