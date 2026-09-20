// Structural checks for the polled API responses.
//
// `api<T>()` casts whatever JSON came back to `T` without looking at it, so
// the type parameter is a claim about the server, not a fact about the
// bytes. The claim holds for the real daemon; it does not hold for the
// things that sit between the two in practice — a proxy error page served
// as 200 with a JSON content type, a captive portal, a truncated body, or
// a daemon mid-rollback answering an older schema.
//
// The previous guard only checked top-level categories: object vs array.
// `{}` is an object, so an empty body passed as a `Summary` and was written
// into the store, and `StatusPage` then read `summary.activity_status.kind`
// off `undefined`.
//
// These validators deliberately check only what the UI actually
// dereferences without a `?.`, which is why they are not a schema mirror:
// a field the components already treat as optional does not need to be
// present for the dashboard to render, and requiring it would turn a
// harmless server addition into a blank page. The rule for adding to this
// file is the same rule that put each line in it — something renders it
// unconditionally.

import type { FindingDetail, Stats, Summary } from "./types";

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isObjectArray(v: unknown): v is Record<string, unknown>[] {
  return Array.isArray(v) && v.every(isObject);
}

/**
 * Each branch of the status panel reads one variant field with no `?.` —
 * `{@const cj = activity_status.job}` followed by `cj.kind` throws on a
 * body that claims `running` and carries no job. A kind nobody branches on
 * renders nothing, so it needs nothing.
 */
function hasActivityFields(a: Record<string, unknown>): boolean {
  switch (a.kind) {
    case "running":
      return isObject(a.job);
    case "paused":
    case "ready":
      return isObject(a.candidate);
    case "error":
      return typeof a.detail === "string";
    default:
      return true;
  }
}

/** `activity_status.kind` drives the whole status panel's branch. */
export function isSummary(v: unknown): v is Summary {
  if (!isObject(v)) return false;
  if (!isObject(v.activity_status) || typeof v.activity_status.kind !== "string") return false;
  if (!hasActivityFields(v.activity_status)) return false;
  if (typeof v.backend_status_html !== "string") return false;
  if (!isObject(v.counts) || !isObject(v.type_counts)) return false;
  // Records, not merely an array: `ReposPage` reads `r.url` and
  // `r.added_at` off every entry to build a repo's identity, and three
  // pages read `r.id`/`r.name` for their repo-name maps — none of them
  // optionally. `repos: [null]` would pass an array check, reach the
  // store, and throw inside a `$derived`. Same rule already applied to
  // `jobs` in `isFindingDetail`.
  return isObjectArray(v.repos);
}

/** `StatsPage` maps `by_kind`/`by_finding` and reads `totals.jobs`. */
export function isStats(v: unknown): v is Stats {
  if (!isObject(v)) return false;
  if (!isObject(v.totals)) return false;
  return isObjectArray(v.by_kind) && isObjectArray(v.by_finding);
}

/**
 * The job table is `{#each detail.jobs as job (job.id)}`, and Svelte 5
 * throws on a repeated key rather than dropping the row: two jobs that
 * both arrive without an `id` key the block on `undefined` twice and take
 * the whole detail panel down. `id` is a number by contract, so the
 * distinctness the keyed block needs is a plain count of the values.
 */
function hasKeyableJobs(jobs: Record<string, unknown>[]): boolean {
  const ids = new Set<unknown>();
  for (const job of jobs) {
    if (typeof job.id !== "number") return false;
    ids.add(job.id);
  }
  return ids.size === jobs.length;
}

/** `FindingDetail` reads `detail.jobs.length` and keys each row by `job.id`. */
export function isFindingDetail(v: unknown): v is FindingDetail {
  if (!isObject(v)) return false;
  if (!isObjectArray(v.jobs) || !hasKeyableJobs(v.jobs)) return false;
  // Nullable by contract, so absent is fine, but a non-object non-null
  // would be read as a record by the PR block.
  return v.pr_state === null || v.pr_state === undefined || isObject(v.pr_state);
}

/** The three polled collections are arrays of records. */
export function isRecordList<T>(v: unknown): v is T[] {
  return isObjectArray(v);
}
