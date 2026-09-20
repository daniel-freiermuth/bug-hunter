/**
 * Filter-selection predicates, kept pure and outside the components.
 *
 * These five functions are the entire semantics of the filter bar, and
 * they have now broken twice — both times because a component changed how
 * the selection Set was managed without checking who depended on the old
 * behaviour. Living here they are directly testable, and there is one
 * implementation rather than one per component.
 *
 * The governing rule: `selected` is NOT a subset of `options`. It is a
 * memory of the user's choices, so it deliberately retains options that
 * have since disappeared from the data — a repo whose findings were all
 * triaged still counts as deselected when it comes back. Every predicate
 * here must therefore reason about membership, never about size.
 */

/** Every currently-offered option is selected. Empty options → false. */
export function allSelected(options: readonly string[], selected: ReadonlySet<string>): boolean {
  return options.length > 0 && options.every((o) => selected.has(o));
}

/** No currently-offered option is selected (stale entries do not count). */
export function noneSelected(options: readonly string[], selected: ReadonlySet<string>): boolean {
  return !options.some((o) => selected.has(o));
}

/**
 * Whether the selector must be rendered.
 *
 * A single-option dimension normally needs no control. The exception is
 * when that option is deselected: the filter then matches nothing, and
 * hiding the control would leave no way back short of a reload.
 */
export function needsSelector(options: readonly string[], selected: ReadonlySet<string>): boolean {
  return options.length > 1 || options.some((o) => !selected.has(o));
}

/**
 * Select each option the first time it is ever seen.
 *
 * New data cannot silently hide itself, while an option the user
 * deselected is already in `seen` and so is never re-added behind their
 * back. `seen` grows monotonically and is the reason this is not simply
 * "select everything".
 */
export function absorb(
  options: readonly string[],
  seen: Set<string>,
  selected: Set<string>,
): void {
  for (const option of options) {
    if (!seen.has(option)) {
      seen.add(option);
      selected.add(option);
    }
  }
}

/**
 * Reset a dimension to "everything currently on offer", in place.
 *
 * Mutates rather than replaces: the Set instance is the reactive value,
 * so replacing it costs every holder its subscription.
 */
export function reselect(selected: Set<string>, options: Iterable<string>): void {
  selected.clear();
  for (const option of options) selected.add(option);
}
