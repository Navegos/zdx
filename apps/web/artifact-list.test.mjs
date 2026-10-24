import { expect, test } from "bun:test";
import { dayLabel, groupArtifacts, sortArtifacts } from "./src/lib/artifactList.ts";

const at = (y, m, d, h = 12, min = 0) => new Date(y, m - 1, d, h, min).toISOString();
const row = (name, modified_at) => ({ name, path: `/a/${name}`, modified_at });
const NOW = new Date(2026, 9, 23, 18, 0).getTime();

const rows = [
  row("balance.html", at(2026, 10, 22, 14, 13)),
  row("gone.html", undefined),
  row("setup.html", at(2026, 10, 21, 13, 35)),
  row("approval.html", at(2026, 10, 21, 14, 22)),
  row("today.html", at(2026, 10, 23, 9, 5)),
];

test("newest first by modified time, files without a time last", () => {
  expect(sortArtifacts(rows, "newest").map((r) => r.name)).toEqual([
    "today.html",
    "balance.html",
    "approval.html",
    "setup.html",
    "gone.html",
  ]);
  expect(sortArtifacts(rows, "oldest").map((r) => r.name)).toEqual([
    "setup.html",
    "approval.html",
    "balance.html",
    "today.html",
    "gone.html",
  ]);
  expect(sortArtifacts(rows, "name").map((r) => r.name)).toEqual([
    "approval.html",
    "balance.html",
    "gone.html",
    "setup.html",
    "today.html",
  ]);
});

test("rows group by local day under Today, Yesterday, and dated headers", () => {
  const groups = groupArtifacts(sortArtifacts(rows, "newest"), "newest", NOW);
  expect(groups.map((g) => [g.label === null ? null : g.label.length > 0, g.rows.length])).toEqual([
    [true, 1],
    [true, 1],
    [true, 2],
    [null, 1],
  ]);
  expect(groups[0].label).toBe("Today");
  expect(groups[1].label).toBe("Yesterday");
  expect(groups[2].label).toBe(dayLabel(Date.parse(at(2026, 10, 21)), NOW));
  expect(groups[2].label).not.toBe("Yesterday");
  expect(groups[3].label).toBeNull();
});

test("name order is one ungrouped list", () => {
  const groups = groupArtifacts(sortArtifacts(rows, "name"), "name", NOW);
  expect(groups).toHaveLength(1);
  expect(groups[0].label).toBeNull();
  expect(groupArtifacts([], "newest", NOW)).toEqual([]);
});

test("another year shows the year", () => {
  expect(dayLabel(new Date(2025, 9, 21).getTime(), NOW)).toContain("2025");
});
