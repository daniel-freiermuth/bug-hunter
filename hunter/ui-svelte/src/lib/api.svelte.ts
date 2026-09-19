// Reactive API store — polls every 5s, exposes Svelte 5 rune state
// via a singleton store object.

import type {
  Summary, Finding, Job, Event, Stats, FindingDetail,
} from "./types";

const POLL_MS = 5000;

// ---------------------------------------------------------------------------
// Fetch wrapper
// ---------------------------------------------------------------------------

interface ApiResult<T> {
  status: number;
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
  return { status: r.status, body };
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

  // Lazy caches (not reactive — components re-fetch on demand).
  findingDetailCache = new Map<number, FindingDetail | "loading" | "error">();
  repoNotesCache = new Map<number, string>();

  #interval: ReturnType<typeof setInterval> | null = null;

  async refresh() {
    try {
      const [s, f, j, e, st] = await Promise.all([
        api<Summary>("/api/summary"),
        api<Finding[]>("/api/findings"),
        api<Job[]>("/api/jobs"),
        api<Event[]>("/api/events"),
        api<Stats>("/api/stats"),
      ]);
      if ([s, f, j, e, st].some((r) => r.status !== 200)) {
        this.error = "API error (non-200 response)";
        return;
      }
      this.summary = s.body;
      this.findings = f.body ?? [];
      this.jobs = j.body ?? [];
      this.events = e.body ?? [];
      this.stats = st.body;
      this.error = null;
    } catch (err) {
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

  async fetchFindingDetail(id: number): Promise<FindingDetail | null> {
    const cached = this.findingDetailCache.get(id);
    if (cached && cached !== "loading" && cached !== "error") return cached;
    this.findingDetailCache.set(id, "loading");
    const r = await api<FindingDetail>(`/api/finding?id=${id}`);
    if (r.status === 200 && r.body) {
      this.findingDetailCache.set(id, r.body);
      return r.body;
    }
    this.findingDetailCache.set(id, "error");
    return null;
  }

  async fetchRepoNotes(id: number): Promise<string> {
    if (this.repoNotesCache.has(id)) return this.repoNotesCache.get(id)!;
    const r = await api<{ notes: string }>(`/api/repo/notes?id=${id}`);
    const notes = r.body?.notes ?? "";
    this.repoNotesCache.set(id, notes);
    return notes;
  }
}

export const store = new HunterStore();
