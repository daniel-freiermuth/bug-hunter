// Reactive API store — polls every 5s, exposes Svelte 5 rune state
// via a singleton store object.

import type {
  Summary, Finding, Job, Event, Stats, FindingDetail,
} from "./types";
import { isFindingDetail, isRecordList, isStats, isSummary } from "./validate";

const POLL_MS = 5000;

// ---------------------------------------------------------------------------
// Fetch wrapper
// ---------------------------------------------------------------------------

interface ApiResult<T> {
  status: number;
  /**
   * 2xx. Prefer this over comparing `status` to a literal: the write
   * endpoints are a mix of 200 and 201 (see hunter-rs/API-CONTRACT-WRITES.md
   * §7), and guessing which is which has already shipped as a bug — a 201
   * from /api/repo/notes read as failure. Compare `status` only where the
   * code carries meaning the caller acts on, such as 202 vs 409 on
   * /api/cycle.
   */
  ok: boolean;
  body: T | null;
}

async function api<T>(path: string, opts?: RequestInit): Promise<ApiResult<T>> {
  const r = await fetch(path, opts);
  let body: T | null = null;
  try {
    body = (await r.json()) as T;
  } catch {
    /* empty body or non-JSON */
  }
  return { status: r.status, ok: r.ok, body };
}

/** POST with JSON body, Content-Type set. */
export async function post<T>(path: string, body: Record<string, unknown>): Promise<ApiResult<T>> {
  return api<T>(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

// ---------------------------------------------------------------------------
// Store singleton (Svelte 5 runes inside a class)
// ---------------------------------------------------------------------------

class HunterStore {
  summary = $state<Summary | null>(null);
  findings = $state<Finding[]>([]);
  jobs = $state<Job[]>([]);
  events = $state<Event[]>([]);
  stats = $state<Stats | null>(null);
  error = $state<string | null>(null);

  // Lazy caches (not reactive — components re-fetch on demand). Detail
  // responses are tagged with the data revision they were read in, see
  // fetchFindingDetail.
  findingDetailCache = new Map<number, { revision: number; detail: FindingDetail }>();
  repoNotesCache = new Map<number, string>();

  #interval: ReturnType<typeof setInterval> | null = null;

  // Guards against a stale in-flight refresh clobbering a newer one when the
  // poll timer and a mutation handler overlap: only the latest generation may
  // write state.
  #refreshGeneration = 0;

  // Bumped only where the polled state above is actually replaced, so a
  // superseded or failing refresh neither invalidates caches nor wakes
  // subscribers. Nothing on the read path calls refresh(), so observing this
  // cannot feed back into a refresh loop.
  #revision = $state(0);

  async refresh() {
    const gen = ++this.#refreshGeneration;
    try {
      const [s, f, j, e, st] = await Promise.all([
        api<Summary>("/api/summary"),
        api<Finding[]>("/api/findings"),
        api<Job[]>("/api/jobs"),
        api<Event[]>("/api/events"),
        api<Stats>("/api/stats"),
      ]);
      if (gen !== this.#refreshGeneration) return;
      if ([s, f, j, e, st].some((r) => r.status !== 200)) {
        this.error = "API error (non-200 response)";
        return;
      }
      // A 200 whose body failed to parse (truncated response, proxy error
      // page, Content-Type mismatch) arrives as null, and a body that parsed
      // to the wrong shape is no more usable. Reject before touching any
      // state: writing these through would swap real data for a confident
      // empty dashboard, and bumping #revision would send every open detail
      // panel refetching against data that never landed.
      //
      // Checking the category alone was not enough to keep that promise:
      // `{}` is an object, so it passed as a Summary and the status panel
      // then dereferenced its missing `activity_status`.
      if (
        !isSummary(s.body)
        || !isStats(st.body)
        || !isRecordList(f.body)
        || !isRecordList(j.body)
        || !isRecordList(e.body)
      ) {
        this.error = "API error (malformed response body)";
        return;
      }
      this.summary = s.body;
      this.findings = f.body;
      this.jobs = j.body;
      this.events = e.body;
      this.stats = st.body;
      this.error = null;
      this.#revision++;
    } catch (err) {
      if (gen !== this.#refreshGeneration) return;
      this.error = String(err);
    }
  }

  startPolling() {
    this.refresh();
    if (!this.#interval) {
      this.#interval = setInterval(() => this.refresh(), POLL_MS);
    }
  }

  stopPolling() {
    if (this.#interval) {
      clearInterval(this.#interval);
      this.#interval = null;
    }
  }

  /**
   * Version of the polled data. Reactive: read it inside an `$effect` to
   * re-run when — and only when — a refresh landed new data.
   */
  get revision(): number {
    return this.#revision;
  }

  async fetchFindingDetail(id: number): Promise<FindingDetail | null> {
    // Tying the cache to the data revision rather than dropping it keeps
    // open/close churn free while guaranteeing the panel never shows job or PR
    // state older than the last 5s poll — a TTL would need its own clock and
    // would still drift from the poll that changed the data.
    const cached = this.findingDetailCache.get(id);
    if (cached && cached.revision === this.#revision) return cached.detail;
    // Captured BEFORE the await: a poll can land while this request is in
    // flight, and tagging the response with the revision current at
    // write time would file data fetched against the old state as though
    // it were the new one — the refresh-triggered reload would then be
    // served this stale body from cache and never see the change.
    const requestedAt = this.#revision;
    const r = await api<FindingDetail>(`/api/finding?id=${id}`);
    // Same reasoning as refresh(): a truthy body is not a usable one, and
    // an unusable one must not reach the cache, where it would be served
    // to every reopen until the next poll.
    if (r.status === 200 && isFindingDetail(r.body)) {
      this.findingDetailCache.set(id, { revision: requestedAt, detail: r.body });
      return r.body;
    }
    this.findingDetailCache.delete(id);
    return null;
  }

  async fetchRepoNotes(id: number): Promise<string> {
    if (this.repoNotesCache.has(id)) return this.repoNotesCache.get(id)!;
    const r = await api<{ notes: string }>(`/api/repo/notes?id=${id}`);
    // An error body carries no `notes`; coercing that to "" and caching it
    // would show "No notes yet" forever with no retry, so fail instead and
    // let the caller's failure path handle it.
    if (!r.ok || typeof r.body?.notes !== "string") {
      throw new Error(`GET /api/repo/notes?id=${id} failed (status ${r.status})`);
    }
    this.repoNotesCache.set(id, r.body.notes);
    return r.body.notes;
  }
}

export const store = new HunterStore();
