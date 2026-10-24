/**
 * Monitor figures shown in more than one place (the Overview tiles and the
 * menu), so they read the same everywhere. All of them cover all of zdx, not
 * one thread.
 */
import type { MonitorJob, MonitorResponse } from "./types";

/** A job whose last run needs a look. */
export function jobFailed(job: MonitorJob): boolean {
  const outcome = job.last_run?.outcome;
  return outcome === "failed" || outcome === "missed" || outcome === "interrupted";
}

/** `running/total`, e.g. `2/2`. */
export function servicesLabel(data: MonitorResponse): string {
  return `${data.services.filter((s) => s.running).length}/${data.services.length}`;
}

/**
 * Highest used share across every quota window. Quotas are fetched off the
 * request path and land on a later poll, so an unfetched snapshot reads as
 * unknown (`—`) rather than as a real 0%.
 */
export function peakQuota(data: MonitorResponse | null): string {
  const used = data?.subscriptions.flatMap((s) => s.windows.map((w) => w.used_percent)) ?? [];
  return used.length ? `${Math.max(...used).toFixed(0)}%` : "—";
}
