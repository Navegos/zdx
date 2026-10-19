import { expect, test } from "bun:test";
import { findLinkedArtifact, isArtifactKey, parseStartParam } from "./src/lib/artifactLink.ts";

const KEY = "0123456789abcdef";
const TOPIC = "telegram--1001234567890-topic-17771";

test("a bare start parameter is still a thread link", () => {
  expect(parseStartParam(TOPIC)).toEqual({ id: TOPIC, artifact: "" });
  expect(parseStartParam("3f2b1c9e-0000-4000-8000-000000000000")).toEqual({
    id: "3f2b1c9e-0000-4000-8000-000000000000",
    artifact: "",
  });
  expect(parseStartParam("  ")).toBeNull();
});

test("an artifact link names the receiving thread and the file key", () => {
  expect(parseStartParam(`a_${KEY}_${TOPIC}`)).toEqual({ id: TOPIC, artifact: KEY });
  // Same shape the bot builds: within Telegram's start-parameter rules.
  expect(`a_${KEY}_${TOPIC}`).toMatch(/^[\w-]{1,64}$/);
});

test("a malformed artifact link never opens a file", () => {
  for (const raw of [`a_${KEY.toUpperCase()}_${TOPIC}`, `a_${KEY.slice(1)}_${TOPIC}`, `a_${KEY}_`, `a_${KEY}`]) {
    expect(parseStartParam(raw)?.artifact).toBe("");
  }
  expect(parseStartParam(`a_${KEY}_active`)).toEqual({ id: `a_${KEY}_active`, artifact: "" });
  expect(isArtifactKey("../etc/passwd")).toBe(false);
  expect(isArtifactKey(KEY)).toBe(true);
});

test("the link opens the row with that key, not one with the same name", () => {
  const items = [
    { path: "/home/a/.zdx/artifacts/threads/t/report.html", name: "report.html", key: "aaaaaaaaaaaaaaaa" },
    { path: "/home/a/.zdx/artifacts/threads/w/report.html", name: "report.html", key: KEY },
  ];
  expect(findLinkedArtifact(items, KEY)?.path).toBe("/home/a/.zdx/artifacts/threads/w/report.html");
  expect(findLinkedArtifact(items, "ffffffffffffffff")).toBeUndefined();
  expect(findLinkedArtifact(items, "")).toBeUndefined();
});
