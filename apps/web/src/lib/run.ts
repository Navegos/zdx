/**
 * Presentation helpers for the shared live run status (`RunStatus`). The
 * server derives state, label and ages; this only formats them.
 */
import type { RunStatus } from "./types";

/** Short age, e.g. `42s`, `3m 5s`, `1h 2m` (same shape as the server's). */
export function formatSecs(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m}m ${secs % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

/**
 * `last event 4s ago · heartbeat 2s ago`, leaving out ages the marker does not
 * have. Reported as observed: a quiet build is not called stuck.
 */
export function runAges(run: RunStatus): string {
  const parts: string[] = [];
  if (run.seconds_since_activity !== null)
    parts.push(`last event ${formatSecs(run.seconds_since_activity)} ago`);
  if (run.seconds_since_heartbeat !== null)
    parts.push(`heartbeat ${formatSecs(run.seconds_since_heartbeat)} ago`);
  return parts.join(" · ");
}
