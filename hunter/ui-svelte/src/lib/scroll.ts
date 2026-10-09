import { findScrollRoot } from "./nearViewport";

/**
 * Space left above an element placed at the top of its scroll container.
 *
 * The browser keeps the first visible element in place while content above
 * it changes size (scroll anchoring), and for a placed card that element
 * has to be the card itself. So the card must be fully visible (a gap
 * above zero, beyond sub-pixel rounding), and the card before it must be
 * fully hidden: with the gap as wide as the list's 0.5rem spacing, the
 * previous card's bottom edge stayed a fraction of a pixel in view, the
 * browser anchored on its placeholder, which mounting removes, and the
 * placed card slid by up to 50px. Keep this below the card-list gap.
 */
export const TOP_GAP_PX = 4;

/**
 * Scroll `el`'s container in one instant jump so that `el` starts `gap`
 * px below the container's top edge; a negative gap leaves its top that
 * far above the edge. Reports whether `el` got there: while lazy cards are
 * still placeholders the page may be too short to reach it, or not scroll
 * at all yet.
 *
 * Cards mounting around `el` change size, and scroll anchoring holds `el`
 * in place while they do (see FindingCard's placeholder).
 */
export function placeAtTop(el: Element, gap = TOP_GAP_PX): boolean {
  const scroller = findScrollRoot(el.parentElement ?? el);
  if (!scroller) return false;
  const offset = () => el.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
  scroller.scrollBy({ top: offset() - gap, behavior: "instant" });
  return Math.abs(offset() - gap) < 1;
}

const USER_SCROLL_EVENTS = ["wheel", "touchstart", "keydown", "pointerdown"] as const;

/**
 * Run `place` now and again whenever `watch` changes size, until it
 * reports success or the user scrolls on their own. Pages fill in from
 * data that may still be arriving and lazy cards grow as they mount, so a
 * position that is out of reach now may be reachable a frame later.
 * Returns a cancel function.
 */
export function settle(watch: Element, place: () => boolean): () => void {
  if (place()) return () => {};
  const observer = new ResizeObserver(() => {
    if (place()) cancel();
  });
  function cancel() {
    observer.disconnect();
    for (const type of USER_SCROLL_EVENTS) window.removeEventListener(type, cancel);
  }
  observer.observe(watch);
  for (const type of USER_SCROLL_EVENTS) window.addEventListener(type, cancel, { passive: true });
  return cancel;
}
