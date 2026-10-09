// Which findings a page shows, and in what order, as a pure function of the
// URL. Pages derive their list from it directly, so a URL change re-renders
// the list in the same update as everything else: nothing waits for an
// effect to push a filtered copy into page state.

import type { FindingOut } from "./types";
import { SEV_RANK } from "./format";
import { Filter } from "./filter.svelte";
import { FILTER_KEYS, readConfidence, readSelection, readSort, type SortBy } from "./filterUrl";

/** What a page filters over, beyond the URL. */
export type FilterScope = {
  repoNames: ReadonlyMap<number, string>;
  /** Only All Findings offers a status filter; elsewhere `status` is ignored. */
  showStatus: boolean;
};

/** Every URL parameter that can hide a finding. */
const DIMENSIONS = [...FILTER_KEYS, "confidence"] as const;
type Dimension = (typeof DIMENSIONS)[number];

/** The filters and sort a URL asks for. */
type View = {
  repo: Filter;
  type: Filter;
  class: Filter;
  severity: Filter;
  status: Filter;
  minConfidence: number;
  sortBy: SortBy;
};

function readView(params: URLSearchParams): View {
  return {
    repo: new Filter(readSelection(params, "repo")),
    type: new Filter(readSelection(params, "type")),
    class: new Filter(readSelection(params, "class")),
    severity: new Filter(readSelection(params, "severity")),
    status: new Filter(readSelection(params, "status")),
    minConfidence: readConfidence(params),
    sortBy: readSort(params),
  };
}

function accepts(view: View, scope: FilterScope, f: FindingOut, dimension: Dimension): boolean {
  switch (dimension) {
    case "repo":
      return view.repo.accepts(scope.repoNames.get(f.repo_id) ?? "unknown");
    case "type":
      return view.type.accepts(f.type);
    case "class":
      return f.category == null || view.class.accepts(f.category);
    case "severity":
      return view.severity.accepts(f.severity);
    case "status":
      return !scope.showStatus || view.status.accepts(f.status);
    case "confidence":
      return f.confidence >= view.minConfidence / 100;
  }
}

/** The URL parameters that hide `finding`, each one on its own. */
export function hidingParams(finding: FindingOut, params: URLSearchParams, scope: FilterScope): Dimension[] {
  const view = readView(params);
  return DIMENSIONS.filter((dimension) => !accepts(view, scope, finding, dimension));
}

/** `findings` as `params` filter and sort them. */
export function filterFindings(
  findings: readonly FindingOut[],
  params: URLSearchParams,
  scope: FilterScope,
): FindingOut[] {
  const view = readView(params);
  const out = findings.filter((f) => DIMENSIONS.every((dimension) => accepts(view, scope, f, dimension)));
  // Sort in place: filter() already returned a fresh array.
  switch (view.sortBy) {
    case "severity":
      out.sort((a, b) => {
        const sd = (SEV_RANK[b.severity] ?? 0) - (SEV_RANK[a.severity] ?? 0);
        if (sd !== 0) return sd;
        return b.confidence - a.confidence;
      });
      break;
    case "newest":
      out.sort((a, b) => b.created_at - a.created_at);
      break;
    case "updated":
      // updated_at, not created_at: a finding moves when it is triaged,
      // fixed, or its PR changes state, so this surfaces what the daemon
      // and you have just been working on rather than what happened to be
      // found last.
      out.sort((a, b) => b.updated_at - a.updated_at);
      break;
    case "oldest":
      out.sort((a, b) => a.created_at - b.created_at);
      break;
  }
  return out;
}
