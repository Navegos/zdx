/**
 * Thread usage read from its activity, shared by the Agent tab and the menu's
 * thread card so both show the same model and context occupancy.
 */
import type { ThreadActivity, UsageActivity } from "./types";

/** Most recent usage event, for model/provider attribution. */
export function lastUsage(activity: readonly ThreadActivity[]): UsageActivity | null {
  for (let i = activity.length - 1; i >= 0; i--) {
    const item = activity[i];
    if (item.type === "usage") return item;
  }
  return null;
}

export interface ContextOccupancy {
  used: number;
  limit: number | null;
  pct: number | null;
}

/**
 * What occupies the model's window now. Providers emit two kinds of usage
 * event: one carrying the prompt (input + cache) and one carrying only the
 * completion. Only the former describes the window, so the plain last event
 * is usually ~0 and useless.
 */
export function contextOccupancy(activity: readonly ThreadActivity[]): ContextOccupancy | null {
  for (let i = activity.length - 1; i >= 0; i--) {
    const item = activity[i];
    if (item.type !== "usage") continue;
    const prompt = item.input_tokens + item.cache_read_tokens + item.cache_write_tokens;
    if (prompt === 0) continue;
    const used = prompt + item.output_tokens;
    const limit = item.context_limit ?? null;
    return { used, limit, pct: limit ? (used / limit) * 100 : null };
  }
  return null;
}
