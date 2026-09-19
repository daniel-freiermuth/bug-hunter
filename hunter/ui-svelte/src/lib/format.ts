// Shared formatting utilities (ported from app.ts helpers).

/** Token count to human-readable: 150000 → "150k", 2100000 → "2.1M" */
export function ktok(n: number | null): string {
  if (n == null) return "\u2013";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return Math.round(n / 1_000) + "k";
  return String(n);
}

/** Epoch ms → local time string "2:05 PM" */
export function ts(ms: number | null): string {
  if (!ms) return "\u2013";
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Epoch ms → "Sep 14 2:05 PM" */
export function datetime(ms: number | null): string {
  if (!ms) return "\u2013";
  const d = new Date(ms);
  return (
    d.toLocaleDateString([], { month: "short", day: "numeric" }) +
    " " +
    d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
  );
}

/** Epoch ms → countdown "2h05m" / "45m" / "-3m" */
export function countdown(ms: number | null): string {
  if (!ms) return "";
  let d = Math.round((ms - Date.now()) / 1000);
  const sign = d < 0 ? "-" : "";
  d = Math.abs(d);
  const h = Math.floor(d / 3600);
  const m = Math.floor((d % 3600) / 60);
  return h > 0 ? `${sign}${h}h${String(m).padStart(2, "0")}m` : `${sign}${m}m`;
}

/** Job duration in seconds. */
export function dur(startedAt: number | null, finishedAt: number | null): string {
  if (!startedAt) return "\u2013";
  const end = finishedAt || Date.now();
  return Math.round((end - startedAt) / 1000) + "s";
}

/** Percentage formatting: 0.76 → "76.00%" */
export function pct(v: number | null): string {
  return v != null ? (v * 100).toFixed(2) + "%" : "\u2013";
}

/** HTML-escape for safe interpolation. */
export function esc(s: unknown): string {
  const map: Record<string, string> = {
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  };
  return String(s ?? "").replace(/[&<>"']/g, (c) => map[c] ?? c);
}

/** Severity rank for sorting. */
export const SEV_RANK: Record<string, number> = { high: 3, medium: 2, low: 1 };

/** Severity CSS color class. */
export function sevColor(sev: string): string {
  switch (sev) {
    case "high":
      return "text-sev-high";
    case "medium":
      return "text-sev-medium";
    case "low":
      return "text-sev-low";
    default:
      return "text-text-dim";
  }
}

/** Type emoji + short label (matches original UI). */
export function typeLabel(t: string): string {
  switch (t) {
    case "bug":
      return "\ud83d\udc1b Bug";
    case "dep_update":
      return "\ud83d\udce6 Dep";
    case "test_gap":
      return "\ud83e\uddea Test";
    case "refactor":
      return "\u267b\ufe0f Refactor";
    case "modernization":
      return "\ud83d\udd2c Modern";
    default:
      return t || "?";
  }
}

/** Type emoji only. */
export function typeEmoji(t: string): string {
  switch (t) {
    case "bug":
      return "\ud83d\udc1b";
    case "dep_update":
      return "\ud83d\udce6";
    case "test_gap":
      return "\ud83e\uddea";
    case "refactor":
      return "\u267b\ufe0f";
    case "modernization":
      return "\ud83d\udd2c";
    default:
      return "\u2753";
  }
}
