// What Settings → System shows, formatted: the pure half of the System tab
// (`screens/SystemTab.tsx`), kept here so it can be tested without a DOM.

import type { GetSystemInfoResponse } from "./client";

/** A byte count in binary units, one decimal place from KiB up: `1.5 GiB`. */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${value.toFixed(1)} ${units[unit]}`;
}

/** A duration as its two largest units: `3d 4h`, `2h 5m`, `42s`. */
export function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = Math.floor(seconds % 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

/** `used of total (percent)`, for memory and disk: what is in use is the
 * total less what is available. `null` when either number is missing. */
export function usage(
  total: number | null | undefined,
  available: number | null | undefined,
): { used: number; total: number; percent: number } | null {
  if (total == null || available == null || total <= 0) return null;
  const used = Math.max(0, total - available);
  return { used, total, percent: Math.round((used / total) * 1000) / 10 };
}

/** The commit as it is usually quoted: the first 12 characters, keeping a
 * `-dirty` suffix, which is the part that says the build had local edits. */
export function shortCommit(commit: string): string {
  const dirty = commit.endsWith("-dirty");
  const hash = dirty ? commit.slice(0, -"-dirty".length) : commit;
  return `${hash.slice(0, 12)}${dirty ? "-dirty" : ""}`;
}

/** Whether the server answering now is a different process from the one that
 * was asked to restart — which is when the restart is over. */
export function restarted(
  before: Pick<GetSystemInfoResponse, "pid" | "started_at">,
  now: Pick<GetSystemInfoResponse, "pid" | "started_at">,
): boolean {
  return now.pid !== before.pid || now.started_at !== before.started_at;
}
