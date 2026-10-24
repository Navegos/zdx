/**
 * Presentation of the Artifacts tab list: sort order and day groups. The
 * server already sends rows newest first by file modified time
 * (`zdx_engine::core::artifacts`); this only re-orders for the reader's
 * choice and groups by local day. No `$lib` imports, so `artifact-list.test.mjs`
 * can load it under `bun test`.
 */
import type { ArtifactItem } from "./types";

export type ArtifactSort = "newest" | "oldest" | "name";

export const ARTIFACT_SORT_LABELS: Record<ArtifactSort, string> = {
  newest: "Newest first",
  oldest: "Oldest first",
  name: "Name A–Z",
};

type Row = Pick<ArtifactItem, "name" | "path" | "modified_at">;

/** Parsed `modified_at`, or null when the row has none. */
export function modifiedTime(row: Row): number | null {
  if (!row.modified_at) return null;
  const t = Date.parse(row.modified_at);
  return Number.isFinite(t) ? t : null;
}

/** Rows in the chosen order. Rows without a time (the file is gone) come last
 *  in both date orders. */
export function sortArtifacts<T extends Row>(rows: readonly T[], sort: ArtifactSort): T[] {
  const byName = (a: T, b: T) => a.name.localeCompare(b.name) || a.path.localeCompare(b.path);
  return [...rows].sort((a, b) => {
    if (sort === "name") return byName(a, b);
    const ta = modifiedTime(a);
    const tb = modifiedTime(b);
    if (ta === null || tb === null) return ta === tb ? byName(a, b) : ta === null ? 1 : -1;
    return (sort === "newest" ? tb - ta : ta - tb) || byName(a, b);
  });
}

function dayStart(t: number): number {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

const weekdayDateFmt = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric" });
const yearDateFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric" });
const clockFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const fullFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "full", timeStyle: "short" });
const shortFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

/** Day header: `Today`, `Yesterday`, `Wed, Oct 21`, or `Oct 21, 2025` in
 *  another year. */
export function dayLabel(t: number, now: number): string {
  const days = Math.round((dayStart(now) - dayStart(t)) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  if (new Date(t).getFullYear() !== new Date(now).getFullYear()) return yearDateFmt.format(t);
  return weekdayDateFmt.format(t);
}

/** `14:13`: the card line, under a day header. */
export function clockLabel(t: number): string {
  return clockFmt.format(t);
}

/** `Oct 21, 14:13`: the card line when the list is not grouped by day. */
export function shortDateLabel(t: number): string {
  return shortFmt.format(t);
}

/** Full local date and time, shown when the reader taps the time. */
export function fullDateLabel(t: number): string {
  return fullFmt.format(t);
}

export interface ArtifactGroup<T> {
  /** Day header, or null for rows without a time. */
  label: string | null;
  rows: T[];
}

/**
 * Consecutive rows of the same local day, in the given order. Rows without a
 * time form one group with a null label. Name order is not grouped: one group,
 * null label.
 */
export function groupArtifacts<T extends Row>(rows: readonly T[], sort: ArtifactSort, now: number): ArtifactGroup<T>[] {
  if (sort === "name") return rows.length ? [{ label: null, rows: [...rows] }] : [];
  const groups: ArtifactGroup<T>[] = [];
  let lastDay: number | null | undefined;
  for (const row of rows) {
    const t = modifiedTime(row);
    const day = t === null ? null : dayStart(t);
    if (groups.length === 0 || day !== lastDay) {
      groups.push({ label: t === null ? null : dayLabel(t, now), rows: [] });
      lastDay = day;
    }
    groups[groups.length - 1].rows.push(row);
  }
  return groups;
}
