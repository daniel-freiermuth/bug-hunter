import { describe, expect, it } from "vitest";
import { absorb, allSelected, needsSelector, noneSelected, reselect } from "./selection";

const set = (...v: string[]) => new Set(v);

describe("allSelected", () => {
  it("is true when every option is selected", () => {
    expect(allSelected(["a", "b"], set("a", "b"))).toBe(true);
  });

  it("is false when an option is missing", () => {
    expect(allSelected(["a", "b"], set("a"))).toBe(false);
  });

  /**
   * Regression. `selected.size === options.length` reported true here, so
   * the UI showed the All box checked and no filter indicator while the
   * filter excluded every finding. Observed live: 1 option (test_gap),
   * 1 stale selection (bug), 0 of 96 shown.
   */
  it("is false when a stale entry makes the sizes match but covers nothing", () => {
    expect(allSelected(["test_gap"], set("bug"))).toBe(false);
  });

  it("is false when there is nothing on offer", () => {
    expect(allSelected([], set("bug"))).toBe(false);
  });
});

describe("noneSelected", () => {
  it("ignores stale entries", () => {
    // "bug" is remembered but not on offer: nothing offered is selected.
    expect(noneSelected(["test_gap"], set("bug"))).toBe(true);
  });

  it("is false when any offered option is selected", () => {
    expect(noneSelected(["a", "b"], set("b"))).toBe(false);
  });
});

describe("needsSelector", () => {
  it("hides a single fully-selected dimension", () => {
    expect(needsSelector(["a"], set("a"))).toBe(false);
  });

  /**
   * Regression. With `options.length > 1` the control vanished exactly
   * when it was needed: the lone option deselected, so the filter matched
   * nothing and there was no way back short of a reload.
   */
  it("shows a single dimension whose only option is deselected", () => {
    expect(needsSelector(["a"], set())).toBe(true);
  });

  it("shows any multi-option dimension", () => {
    expect(needsSelector(["a", "b"], set("a", "b"))).toBe(true);
  });
});

describe("absorb", () => {
  it("selects options the first time they are seen", () => {
    const seen = set();
    const selected = set();
    absorb(["a", "b"], seen, selected);
    expect([...selected].sort()).toEqual(["a", "b"]);
  });

  it("does not re-add an option the user deselected", () => {
    const seen = set();
    const selected = set();
    absorb(["a", "b"], seen, selected);
    selected.delete("b"); // user deselects
    absorb(["a", "b"], seen, selected); // later refresh
    expect([...selected]).toEqual(["a"]);
  });

  it("absorbs a genuinely new option alongside an existing deselection", () => {
    const seen = set();
    const selected = set();
    absorb(["a"], seen, selected);
    selected.delete("a");
    absorb(["a", "c"], seen, selected);
    expect([...selected]).toEqual(["c"]);
  });

  it("keeps a selection for an option that disappears and returns", () => {
    const seen = set();
    const selected = set();
    absorb(["a", "b"], seen, selected);
    absorb(["a"], seen, selected); // b drops out of the data
    expect(selected.has("b")).toBe(true); // remembered, not dropped
    absorb(["a", "b"], seen, selected); // b returns
    expect([...selected].sort()).toEqual(["a", "b"]);
  });

  it("mutates in place rather than replacing the instance", () => {
    const selected = set();
    const same = selected;
    absorb(["a"], set(), selected);
    expect(same).toBe(selected);
    expect(same.has("a")).toBe(true);
  });
});

describe("reselect", () => {
  it("drops stale entries and selects everything on offer", () => {
    const selected = set("bug");
    reselect(selected, ["test_gap", "refactor"]);
    expect([...selected].sort()).toEqual(["refactor", "test_gap"]);
  });

  it("mutates in place", () => {
    const selected = set("x");
    const same = selected;
    reselect(selected, ["y"]);
    expect(same).toBe(selected);
  });
});

describe("clearing filters restores options that have come and gone", () => {
  it("re-selects an option that vanished and returned", () => {
    const known = new Set<string>();
    const selected = new Set<string>();

    // A repo appears and is absorbed as selected.
    absorb(["alpha", "beta"], known, selected);
    expect([...selected].sort()).toEqual(["alpha", "beta"]);

    // Its last finding is triaged away, so it drops out of the corpus, and
    // the user clears every filter while it is absent.
    reselect(selected, known);

    // It comes back. `known` still contains it, so absorb() will not
    // re-select it — clearing must already have, or its findings stay
    // hidden from a user who asked to see everything.
    absorb(["alpha", "beta"], known, selected);
    expect(selected.has("beta")).toBe(true);
  });

  it("clearing from the present options alone loses the absent one", () => {
    // The shape of the bug, pinned so the fix is not silently reverted.
    const known = new Set<string>();
    const selected = new Set<string>();
    absorb(["alpha", "beta"], known, selected);

    reselect(selected, ["alpha"]); // only what is present right now
    absorb(["alpha", "beta"], known, selected); // beta returns

    expect(selected.has("beta")).toBe(false);
  });
});
