export const FILTER_KEYS = ["repo", "type", "class", "severity", "status"] as const;
export type FilterKey = (typeof FILTER_KEYS)[number];
export const SORT_OPTIONS = ["severity", "newest", "updated", "oldest"] as const;
export type SortBy = (typeof SORT_OPTIONS)[number];

/** Missing means all; a present empty value means none. Repeated keys select multiple values. */
export function readSelection(params: URLSearchParams, key: FilterKey): string[] | null {
  return params.has(key) ? params.getAll(key).filter((value) => value !== "") : null;
}

/** Replace one selection while preserving unrelated parameters; null removes it, [] records none. */
export function writeSelection(params: URLSearchParams, key: FilterKey, values: readonly string[] | null): void {
  params.delete(key);
  if (values === null) return;
  if (values.length === 0) params.set(key, "");
  else for (const value of values) params.append(key, value);
}

/** Accept only slider-representable confidence thresholds; malformed links restore zero. */
export function readConfidence(params: URLSearchParams): number {
  const value = Number(params.get("confidence") ?? 0);
  return Number.isInteger(value) && value >= 0 && value <= 100 && value % 5 === 0 ? value : 0;
}

/** Restore a supported ordering, falling back to severity for unknown link values. */
export function readSort(params: URLSearchParams): SortBy {
  const value = params.get("sort");
  return SORT_OPTIONS.find((option) => option === value) ?? "severity";
}
