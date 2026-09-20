import { describe, expect, it } from "vitest";

import { isFindingDetail, isRecordList, isStats, isSummary } from "./validate";

// A body shaped like what the daemon actually serves, trimmed to the
// fields the validators require.
const summary = {
  backend_status_html: "<div></div>",
  counts: { new: 1 },
  type_counts: { bug: 1 },
  repos: [],
  last_cycle: null,
  cycle_running: false,
  current_job: null,
  next_candidate: null,
  scheduler_state: null,
  activity_status: { kind: "idle" },
};

describe("isSummary", () => {
  it("accepts a real summary", () => {
    expect(isSummary(summary)).toBe(true);
  });

  // The case that motivated the validator: an empty object is an object,
  // so the old category check passed it through to the status panel.
  it("rejects an empty object", () => {
    expect(isSummary({})).toBe(false);
  });

  it("rejects a summary whose activity_status is missing or unusable", () => {
    expect(isSummary({ ...summary, activity_status: undefined })).toBe(false);
    expect(isSummary({ ...summary, activity_status: null })).toBe(false);
    // Present but without the discriminant every branch in StatusPage reads.
    expect(isSummary({ ...summary, activity_status: {} })).toBe(false);
  });

  // StatusPage's branches read these with no `?.`, so a body that claims
  // the kind without the field renders straight into a throw.
  const variants = [
    ["running", "job", { id: 1, kind: "hunt", repo_name: "r" }],
    ["paused", "candidate", { kind: "fix", id: 2, label: "x" }],
    ["ready", "candidate", { kind: "fix", id: 2, label: "x" }],
    ["error", "detail", "clone failed"],
  ] as const;

  it.each(variants)("requires %s to carry %s", (kind, field, value) => {
    expect(isSummary({ ...summary, activity_status: { kind, [field]: value } })).toBe(true);
    expect(isSummary({ ...summary, activity_status: { kind } })).toBe(false);
  });

  it("rejects a variant field of the wrong shape", () => {
    expect(isSummary({ ...summary, activity_status: { kind: "running", job: "hunt" } })).toBe(false);
    expect(isSummary({ ...summary, activity_status: { kind: "ready", candidate: [] } })).toBe(false);
    expect(isSummary({ ...summary, activity_status: { kind: "error", detail: null } })).toBe(false);
  });

  // The kinds whose branches render no variant field, plus a kind no
  // branch matches: all render without dereferencing anything.
  it.each(["working", "idle", "warming_up", "hibernating"])(
    "accepts a bare %s activity status",
    (kind) => {
      expect(isSummary({ ...summary, activity_status: { kind } })).toBe(true);
    },
  );

  it("rejects an array, which is also typeof object", () => {
    expect(isSummary([])).toBe(false);
  });

  // Server growth must not blank the dashboard.
  it("accepts a summary carrying fields the client does not know", () => {
    expect(isSummary({ ...summary, something_new: 42 })).toBe(true);
  });
});

describe("isStats", () => {
  const stats = { totals: { jobs: 0 }, by_kind: [], by_finding: [] };

  it("accepts real stats", () => {
    expect(isStats(stats)).toBe(true);
  });

  it("rejects an empty object", () => {
    expect(isStats({})).toBe(false);
  });

  it("rejects collections that are not arrays of records", () => {
    expect(isStats({ ...stats, by_kind: {} })).toBe(false);
    expect(isStats({ ...stats, by_finding: ["nope"] })).toBe(false);
  });
});

describe("isFindingDetail", () => {
  it("accepts a detail with jobs and either shape of pr_state", () => {
    expect(isFindingDetail({ jobs: [], pr_state: null })).toBe(true);
    expect(isFindingDetail({ jobs: [], pr_state: { number: 1 } })).toBe(true);
    expect(isFindingDetail({ jobs: [] })).toBe(true);
  });

  // FindingDetail.svelte keys the job table by job.id.
  it("rejects a detail whose jobs holds a non-object", () => {
    expect(isFindingDetail({ jobs: [{ id: 1 }] })).toBe(true);
    expect(isFindingDetail({ jobs: [null] })).toBe(false);
    expect(isFindingDetail({ jobs: [1] })).toBe(false);
  });

  // FindingDetail.svelte reads detail.jobs.length with no guard.
  it("rejects a detail whose jobs is missing or not an array", () => {
    expect(isFindingDetail({ pr_state: null })).toBe(false);
    expect(isFindingDetail({ jobs: null })).toBe(false);
    expect(isFindingDetail({ jobs: 3 })).toBe(false);
  });

  it("rejects a non-object body", () => {
    expect(isFindingDetail("ok")).toBe(false);
    expect(isFindingDetail(null)).toBe(false);
  });
});

describe("isRecordList", () => {
  it("accepts an empty list and a list of records", () => {
    expect(isRecordList([])).toBe(true);
    expect(isRecordList([{ id: 1 }])).toBe(true);
  });

  it("rejects a non-array and a list holding non-records", () => {
    expect(isRecordList({})).toBe(false);
    expect(isRecordList([1, 2])).toBe(false);
    expect(isRecordList([null])).toBe(false);
  });
});
