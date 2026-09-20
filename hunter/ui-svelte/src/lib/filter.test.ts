import { describe, expect, it } from "vitest";
import { Filter, needsSelector } from "./filter.svelte";

describe("a fresh dimension", () => {
  it("accepts everything, including options nobody has seen yet", () => {
    const f = new Filter();
    expect(f.accepts("hunt")).toBe(true);
    expect(f.accepts("a type that did not exist a moment ago")).toBe(true);
    expect(f.allSelected(["hunt", "fix"])).toBe(true);
    expect(f.filtering).toBe(false);
  });

  it("reads as unfiltered when nothing is on offer at all", () => {
    // "All of nothing" is not a filtered state: a dimension whose data
    // has not arrived must not render as though something were excluded.
    const f = new Filter();
    expect(f.allSelected([])).toBe(true);
    expect(f.noneSelected([])).toBe(false);
  });
});

describe("deselecting All", () => {
  const options = ["bug", "perf", "style"];

  it("excludes everything on offer", () => {
    const f = new Filter();
    f.toggleAll(options);
    expect(f.noneSelected(options)).toBe(true);
    expect(f.allSelected(options)).toBe(false);
    expect(options.some((o) => f.accepts(o))).toBe(false);
  });

  it("keeps excluding an option that appears afterwards", () => {
    // The reported bug. A dimension switched off would let the next
    // newly-seen option in, so the All box turned indeterminate with no
    // user action and a filtered-out finding appeared in the list.
    const f = new Filter();
    f.toggleAll(options);

    const withNewcomer = [...options, "brand_new_type"];
    expect(f.accepts("brand_new_type")).toBe(false);
    expect(f.noneSelected(withNewcomer)).toBe(true);
    expect(f.allSelected(withNewcomer)).toBe(false);
  });

  it("is undone by pressing All again", () => {
    const f = new Filter();
    f.toggleAll(options);
    f.toggleAll(options);
    expect(f.allSelected(options)).toBe(true);
    expect(f.filtering).toBe(false);
    // And unfiltered means unfiltered: new options are welcome again.
    expect(f.accepts("brand_new_type")).toBe(true);
  });

  it("turns an indeterminate dimension into all, not into none", () => {
    // Half-selected is not "on", so the All box offers the completing
    // action rather than wiping what is already chosen.
    const f = new Filter();
    f.toggle("perf", options);
    expect(f.allSelected(options)).toBe(false);
    expect(f.noneSelected(options)).toBe(false);

    f.toggleAll(options);
    expect(f.allSelected(options)).toBe(true);
  });
});

describe("toggling one option", () => {
  const options = ["bug", "perf", "style"];

  it("narrows an unfiltered dimension to the others", () => {
    const f = new Filter();
    f.toggle("perf", options);
    expect(f.accepts("bug")).toBe(true);
    expect(f.accepts("style")).toBe(true);
    expect(f.accepts("perf")).toBe(false);
    expect(f.filtering).toBe(true);
  });

  it("does not let a new option in behind an explicit choice", () => {
    // Same guarantee as deselecting All: once the user has named what
    // they want, later arrivals are not silently added to it.
    const f = new Filter();
    f.toggle("perf", options);
    expect(f.accepts("brand_new_type")).toBe(false);
  });

  it("puts an option back", () => {
    const f = new Filter();
    f.toggle("perf", options);
    f.toggle("perf", options);
    expect(f.accepts("perf")).toBe(true);
    expect(f.allSelected(options)).toBe(true);
    // And it is unfiltered again, not merely holding every option that
    // happens to exist right now: undoing the only exclusion has to
    // restore the promise that new data shows itself.
    expect(f.filtering).toBe(false);
    expect(f.accepts("brand_new_type")).toBe(true);
  });

  it("stays narrowed while any exclusion remains", () => {
    const f = new Filter();
    f.toggle("perf", options);
    f.toggle("style", options);
    f.toggle("perf", options);
    // "style" is still excluded, so this is still an explicit choice
    // and a newcomer is not silently added to it.
    expect(f.filtering).toBe(true);
    expect(f.accepts("perf")).toBe(true);
    expect(f.accepts("style")).toBe(false);
    expect(f.accepts("brand_new_type")).toBe(false);
  });

  it("remembers a choice while its option is absent from the data", () => {
    // A repo whose findings are all triaged drops out of the options and
    // comes back later; it must return still chosen, or the dimension
    // quietly changes meaning while the data moves underneath it.
    const f = new Filter();
    f.toggle("style", options);
    expect(f.accepts("bug")).toBe(true);

    const shrunk = ["perf", "style"];
    expect(f.allSelected(shrunk)).toBe(false);
    expect(f.accepts("bug")).toBe(true);
    expect(f.allSelected(options)).toBe(false);
  });

  it("counts only what is on offer when deciding none", () => {
    const f = new Filter();
    f.toggle("bug", options);
    f.toggle("perf", options);
    f.toggle("style", options);
    expect(f.noneSelected(options)).toBe(true);
    // "bug" is still remembered, so it does not read as none where it is offered.
    f.toggle("bug", options);
    expect(f.noneSelected(["bug"])).toBe(false);
    expect(f.noneSelected(["perf"])).toBe(true);
  });
});

describe("reset", () => {
  it("returns the dimension to accepting everything", () => {
    const f = new Filter();
    f.toggle("perf", ["bug", "perf"]);
    f.reset();
    expect(f.allSelected(["bug", "perf"])).toBe(true);
    expect(f.accepts("perf")).toBe(true);
    // Including options that were never on offer when the user filtered:
    // clearing filters must not leave anything hidden.
    expect(f.accepts("arrived_later")).toBe(true);
  });
});

describe("needsSelector", () => {
  it("hides a single fully-selected dimension", () => {
    expect(needsSelector(["bug"], new Filter())).toBe(false);
  });

  it("shows a single dimension whose only option is excluded", () => {
    // Otherwise the filter matches nothing and there is no way back
    // short of reloading the page.
    const f = new Filter();
    f.toggleAll(["bug"]);
    expect(needsSelector(["bug"], f)).toBe(true);
  });

  it("shows any multi-option dimension", () => {
    expect(needsSelector(["bug", "perf"], new Filter())).toBe(true);
  });
});
