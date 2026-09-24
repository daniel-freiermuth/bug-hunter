import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { POLL_MS, store } from "./api.svelte";

// Bodies shaped like what the daemon serves, trimmed to what the
// validators in validate.ts require — a refresh that fails validation
// writes nothing, which would make every assertion below vacuous.
let servedCounts: Record<string, number>;

function bodyFor(path: string): unknown {
  switch (path) {
    case "/api/summary":
      return {
        backend_status_html: "<div></div>",
        counts: servedCounts,
        type_counts: { bug: 1 },
        repos: [],
        last_cycle: null,
        cycle_running: false,
        current_job: null,
        next_candidate: null,
        scheduler_state: null,
        activity_status: { kind: "idle" },
      };
    case "/api/stats":
      return { totals: { jobs: 0 }, by_kind: [], by_finding: [] };
    default:
      return [];
  }
}

/**
 * A network that answers nothing until told to. Every request is parked
 * in FIFO order; `release(n)` completes the n oldest, which is how a
 * refresh is held open across a poll tick.
 */
interface FakeNet {
  paths: string[];
  release(count: number): Promise<void>;
  /** Complete the `count` NEWEST requests, leaving older ones parked. */
  releaseNewest(count: number): Promise<void>;
}

function installNet(): FakeNet {
  const paths: string[] = [];
  const parked: (() => void)[] = [];
  vi.stubGlobal("fetch", (input: string) => {
    paths.push(input);
    // Captured at request time, like a real server read: a response that
    // set out before a write cannot carry that write's result.
    const body = bodyFor(input);
    return new Promise<Response>((resolve) => {
      parked.push(() => resolve({
        status: 200,
        ok: true,
        json: () => Promise.resolve(body),
      } as unknown as Response));
    });
  });
  // Drain the await chain inside refresh(): fetch, then json(), then
  // Promise.all, then the state writes. Microtasks only — none of it
  // depends on the faked clock.
  const settle = async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve();
  };
  return {
    paths,
    async release(count: number) {
      for (const answer of parked.splice(0, count)) answer();
      await settle();
    },
    async releaseNewest(count: number) {
      for (const answer of parked.splice(-count)) answer();
      await settle();
    },
  };
}

// One refresh fans out to /api/summary, /api/findings, /api/jobs,
// /api/events and /api/stats.
const BATCH = 5;

describe("poll scheduling", () => {
  let net: FakeNet;

  beforeEach(() => {
    // `store` is a module singleton; clear what the previous test wrote
    // so an assertion here can only pass on data this test landed.
    store.summary = null;
    store.findings = [];
    store.jobs = [];
    store.events = [];
    store.stats = null;
    store.error = null;
    servedCounts = { new: 1 };
    net = installNet();
    vi.useFakeTimers();
  });

  afterEach(async () => {
    store.stopPolling();
    // Let any refresh still parked finish, so the next test does not
    // start with a poll the store believes is in flight.
    await net.release(Number.POSITIVE_INFINITY);
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("issues no new requests while a poll refresh is still in flight", async () => {
    store.startPolling();
    expect(net.paths).toHaveLength(BATCH);

    await vi.advanceTimersByTimeAsync(POLL_MS * 3);

    expect(net.paths).toHaveLength(BATCH);
  });

  it("lands a refresh slower than the interval, then resumes polling", async () => {
    store.startPolling();
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);

    await net.release(BATCH);

    // The ticks that fired meanwhile must not have superseded this
    // result: a discarded refresh leaves the dashboard frozen with
    // `error` null, so App.svelte shows no staleness banner either.
    expect(store.summary?.counts).toEqual({ new: 1 });
    expect(store.error).toBeNull();

    // And the gate must reopen once the slow poll settles, or polling
    // stops for good.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(net.paths).toHaveLength(BATCH * 2);
  });

  it("runs a mutation refresh immediately and lets it win", async () => {
    store.startPolling();
    expect(net.paths).toHaveLength(BATCH);

    // A user action wrote; the poll already in flight left before that
    // write and will answer with pre-write data.
    servedCounts = { new: 42 };
    const mutation = store.refresh();
    expect(net.paths).toHaveLength(BATCH * 2);

    // The mutation answers FIRST, then the older poll. Releasing the
    // poll first would let the mutation's response land last and write
    // 42 by arrival order alone, so the assertion would hold with or
    // without the generation guard -- the exact thing under test.
    await net.releaseNewest(BATCH);
    await mutation;
    expect(store.summary?.counts).toEqual({ new: 42 });

    // Now the stale poll, carrying pre-write data. It must not win: a
    // response that set out before the write cannot be allowed to
    // overwrite the write's result.
    await net.release(BATCH);
    expect(store.summary?.counts).toEqual({ new: 42 });
  });
});
