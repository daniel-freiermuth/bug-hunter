import { describe, expect, it } from "vitest";
import { filterFindings, hidingParams, type FilterScope } from "./findingFilter";
import type { FindingOut } from "./types";

function finding(over: Partial<FindingOut>): FindingOut {
  return {
    id: 1,
    repo_id: 1,
    type: "bug",
    category: null,
    severity: "medium",
    status: "new",
    confidence: 0.8,
    created_at: 0,
    updated_at: 0,
    ...over,
  } as FindingOut;
}

const scope: FilterScope = { repoNames: new Map([[1, "alpha"], [2, "beta"]]), showStatus: true };
const ids = (list: FindingOut[]) => list.map((f) => f.id);

describe("filterFindings", () => {
  it("keeps a finding only when every parameter accepts it", () => {
    const list = [
      finding({ id: 1, repo_id: 1, type: "bug" }),
      finding({ id: 2, repo_id: 2, type: "bug" }),
      finding({ id: 3, repo_id: 1, type: "test_gap" }),
    ];
    expect(ids(filterFindings(list, new URLSearchParams("repo=alpha&type=bug"), scope))).toEqual([1]);
  });

  it("ignores the status parameter on pages without a status filter", () => {
    const list = [finding({ id: 1, status: "queued" })];
    const params = new URLSearchParams("status=new");
    expect(ids(filterFindings(list, params, scope))).toEqual([]);
    expect(ids(filterFindings(list, params, { ...scope, showStatus: false }))).toEqual([1]);
  });

  it("never hides an unclassified finding by class", () => {
    const list = [finding({ id: 1, category: null }), finding({ id: 2, category: "logic" })];
    expect(ids(filterFindings(list, new URLSearchParams("class="), scope))).toEqual([1]);
  });

  it("keeps confidence exactly at the threshold", () => {
    const list = [finding({ id: 1, confidence: 0.8 }), finding({ id: 2, confidence: 0.79 })];
    expect(ids(filterFindings(list, new URLSearchParams("confidence=80"), scope))).toEqual([1]);
  });

  it("sorts by severity then confidence by default, and by the requested order otherwise", () => {
    const list = [
      finding({ id: 1, severity: "low", confidence: 0.9, created_at: 3, updated_at: 1 }),
      finding({ id: 2, severity: "high", confidence: 0.6, created_at: 1, updated_at: 3 }),
      finding({ id: 3, severity: "high", confidence: 0.9, created_at: 2, updated_at: 2 }),
    ];
    const by = (query: string) => ids(filterFindings(list, new URLSearchParams(query), scope));
    expect(by("")).toEqual([3, 2, 1]);
    expect(by("sort=newest")).toEqual([1, 3, 2]);
    expect(by("sort=oldest")).toEqual([2, 3, 1]);
    expect(by("sort=updated")).toEqual([2, 3, 1]);
  });
});

describe("hidingParams", () => {
  it("names exactly the parameters that reject the finding", () => {
    const f = finding({ repo_id: 1, type: "bug", status: "rejected", confidence: 0.5 });
    const params = new URLSearchParams("repo=alpha&type=bug&status=new&confidence=80&sort=newest");
    expect(hidingParams(f, params, scope)).toEqual(["status", "confidence"]);
  });
});
