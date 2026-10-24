import { expect, test } from "bun:test";
import {
  HELPER_NS,
  LIMITS,
  TREE_LIMITS,
  buildPayload,
  injectHelper,
  marksFor,
  headUnits,
  parseHelperEvent,
  tailUnits,
  targetSummary,
  wellFormed,
} from "./src/lib/annotate.ts";

const NONCE = "0123456789abcdef0123456789abcdef";
const msg = (extra) => ({ [HELPER_NS]: 1, nonce: NONCE, ...extra });
const element = { selector: "#a", tag: "DIV", text: "t", html: "<div>", rect: { x: 1, y: 2, width: 3, height: 4 } };

test("bridge goes right after the doctype, or first when there is none", () => {
  const withDoctype = injectHelper("<!DOCTYPE html><html><head><meta charset=utf-8></head></html>", NONCE);
  expect(withDoctype.startsWith("<!DOCTYPE html><script>")).toBe(true);
  expect(withDoctype).toContain(NONCE);
  expect(withDoctype).not.toContain("Object.assign(");
  expect(injectHelper("<p>x</p>", NONCE).startsWith("<script>")).toBe(true);
  // Comments, whitespace, and a BOM may precede the doctype without quirks mode.
  const commented = injectHelper("\uFEFF<!-- generated -->\n<!doctype html><html></html>", NONCE);
  expect(commented.startsWith("\uFEFF<!-- generated -->\n<!doctype html><script>")).toBe(true);
  // Text before the doctype already means quirks mode; the bridge goes first.
  expect(injectHelper("x<!doctype html>", NONCE).startsWith("<script>")).toBe(true);
  expect(() => injectHelper("<p>x</p>", "not-hex</script>")).toThrow();
});

test("only well-formed messages from this load's bridge are accepted", () => {
  expect(parseHelperEvent(msg({ type: "ready" }), NONCE)?.type).toBe("ready");
  expect(parseHelperEvent(msg({ type: "ready", nonce: "f".repeat(32) }), NONCE)).toBeNull();
  expect(parseHelperEvent({ ...msg({ type: "ready" }), [HELPER_NS]: 2 }, NONCE)).toBeNull();
  expect(parseHelperEvent(msg({ type: "selected", element }), NONCE)).toBeNull();
  expect(parseHelperEvent(msg({ type: "send" }), NONCE)).toBeNull();
  expect(parseHelperEvent([msg({ type: "ready" })], NONCE)).toBeNull();
  expect(parseHelperEvent(JSON.parse(`{"__proto__":{"type":"ready"},"${HELPER_NS}":1,"nonce":"${NONCE}"}`), NONCE)).toBeNull();
  expect(parseHelperEvent(msg({ type: "picked" }), NONCE)).toBeNull();
});

test("element data is bounded and malformed geometry is dropped", () => {
  const picked = parseHelperEvent(
    msg({ type: "picked", id: 1, element: { ...element, text: "x".repeat(10_000), tag: "DIV onclick=1" } }),
    NONCE,
  );
  expect(picked.element.text.length).toBe(600);
  expect(picked.element.tag).toBe("divonclick1");
  const bad = parseHelperEvent(msg({ type: "picked", id: 1, element: { ...element, rect: { x: NaN, y: 0, width: 1, height: 1 } } }), NONCE);
  expect(bad.element).toBeNull();
  const probed = parseHelperEvent(msg({ type: "probed", id: 2, elements: Array(50).fill(element) }), NONCE);
  expect(probed.elements.length).toBe(12);
  const status = parseHelperEvent(msg({ type: "marks-status", unresolved: [1, "2", 3.5, -1, 999] }), NONCE);
  expect(status.unresolved).toEqual([1]);
});

test("marks sent into the page carry geometry only, never comments", () => {
  const drafts = [
    { id: "a", intent: "change", comment: "secret comment", target: { kind: "element", element }, voice: [], images: [] },
    { id: "b", intent: "note", comment: "page note", target: { kind: "page" }, voice: [], images: [] },
  ];
  const marks = marksFor(drafts);
  expect(marks).toEqual([{ n: 1, selector: "#a" }]);
  expect(JSON.stringify(marks)).not.toContain("secret");
});

test("selection trees are bounded, untrusted text, and tied to the selected node", () => {
  const node = (n, extra = {}) => ({ node: n, tag: "DIV", label: ".card", text: "t", children: 2, ...extra });
  const picked = parseHelperEvent(
    msg({
      type: "picked",
      id: 3,
      element,
      node: 5,
      tree: {
        ancestors: Array.from({ length: 20 }, (_, i) => node(100 + i)),
        self: node(5, { label: "<img src=x onerror=alert(1)>".repeat(5), tag: "SECTION onclick" }),
        children: [...Array.from({ length: 30 }, (_, i) => node(200 + i)), { node: -1 }, { node: 1.5 }, { node: "7" }],
        child_count: 40,
      },
    }),
    NONCE,
  );
  expect(picked.node).toBe(5);
  expect(picked.tree.ancestors.length).toBe(TREE_LIMITS.ancestors);
  // The nearest ancestors are kept.
  expect(picked.tree.ancestors.at(-1).node).toBe(119);
  expect(picked.tree.children.length).toBe(TREE_LIMITS.children);
  expect(picked.tree.childCount).toBe(40);
  expect(picked.tree.self.label.length).toBe(TREE_LIMITS.label);
  expect(picked.tree.self.tag).toBe("sectiononclick");

  // A tree that does not describe the reported node is dropped, and with it the selection.
  const mismatched = parseHelperEvent(msg({ type: "picked", id: 3, element, node: 6, tree: { self: node(5) } }), NONCE);
  expect(mismatched.node).toBeNull();
  expect(mismatched.tree).toBeNull();
  const noTree = parseHelperEvent(msg({ type: "picked", id: 3, element, node: 5 }), NONCE);
  expect(noTree.node).toBeNull();

  for (const state of ["ok", "hidden", "gone"]) {
    expect(parseHelperEvent(msg({ type: "selection-status", node: 5, state }), NONCE)).toEqual({
      type: "selection-status",
      node: 5,
      state,
    });
  }
  expect(parseHelperEvent(msg({ type: "selection-status", node: "5", state: "gone" }), NONCE)).toBeNull();
  expect(parseHelperEvent(msg({ type: "selection-status", node: 5, state: "lost" }), NONCE)).toBeNull();
  expect(parseHelperEvent(msg({ type: "selection-status", node: 5, lost: true }), NONCE)).toBeNull();
});

test("repeated or looping tree ids are reduced to unique nodes", () => {
  const node = (n) => ({ node: n, tag: "div", label: "", text: "", children: 1 });
  const picked = parseHelperEvent(
    msg({
      type: "picked",
      id: 1,
      element,
      node: 5,
      tree: {
        // A page that loops its ancestry repeats the same few ids, and can
        // even report the selection as its own ancestor or child.
        ancestors: [node(1), node(2), node(3), node(2), node(3), node(5), node(2), node(3)],
        self: node(5),
        children: [node(7), node(7), node(5), node(3), node(8), node(7)],
        child_count: 6,
      },
    }),
    NONCE,
  );
  const ids = (list) => list.map((n) => n.node);
  // Nearest first: 3 and 2 from the end, then 1; the selection itself is dropped.
  expect(ids(picked.tree.ancestors)).toEqual([1, 2, 3]);
  // Children repeat neither each other, the selection, nor an ancestor.
  expect(ids(picked.tree.children)).toEqual([7, 8]);
  const all = [...ids(picked.tree.ancestors), picked.tree.self.node, ...ids(picked.tree.children)];
  expect(new Set(all).size).toBe(all.length);
});

test("a text selection report is bounded and needs an element, a box, and words", () => {
  const selection = {
    quote: "x".repeat(10_000),
    prefix: "p".repeat(500),
    suffix: "s".repeat(500),
    element,
    rect: { x: 10, y: 20, width: 30, height: 12 },
  };
  const ok = parseHelperEvent(msg({ type: "text-selected", id: 4, selection }), NONCE);
  expect(ok.type).toBe("text-selected");
  expect(ok.selection.quote.length).toBe(LIMITS.quoteChars);
  expect(ok.selection.prefix.length).toBe(LIMITS.quoteContextChars);
  expect(ok.selection.suffix.length).toBe(LIMITS.quoteContextChars);
  // Nothing selected, blank words, or broken geometry: no selection, not an error.
  expect(parseHelperEvent(msg({ type: "text-selected", id: 4, selection: null }), NONCE).selection).toBeNull();
  expect(parseHelperEvent(msg({ type: "text-selected", id: 4, selection: { ...selection, quote: "  " } }), NONCE).selection).toBeNull();
  expect(
    parseHelperEvent(msg({ type: "text-selected", id: 4, selection: { ...selection, rect: { x: 0 } } }), NONCE).selection,
  ).toBeNull();
  expect(parseHelperEvent(msg({ type: "text-selected", selection }), NONCE)).toBeNull();
});

test("a text note follows its element, sends its quote, and never sends the comment into the page", async () => {
  const target = {
    kind: "text",
    quote: "exact words",
    prefix: "before ",
    suffix: " after",
    element,
    rect: { x: 11, y: 22, width: 30, height: 12 },
  };
  const drafts = [{ id: "t", intent: "question", comment: "secret comment", target, voice: [], images: [] }];
  const marks = marksFor(drafts);
  expect(marks).toEqual([{ n: 1, selector: "#a", offset: { x: 10, y: 20, width: 30, height: 12 } }]);
  expect(JSON.stringify(marks)).not.toContain("secret");
  expect(JSON.stringify(marks)).not.toContain("exact words");
  expect(targetSummary(target)).toBe("“exact words”");

  const payload = await buildPayload({
    submissionId: "sub-12345678",
    artifactPath: "/a/report.html",
    sha256: "a".repeat(64),
    allowStale: false,
    viewport: { width: 390, height: 700, scroll_x: 0, scroll_y: 0 },
    drafts,
  });
  expect(payload.annotations[0].target).toEqual(target);
});

/** True when every string in a value has no lone surrogate: what the
 *  server's JSON parser (serde_json) requires of the submission. */
function noLoneSurrogate(json) {
  return !/\\ud[89ab][0-9a-f]{2}(?!\\ud[c-f][0-9a-f]{2})|(?<!\\ud[89ab][0-9a-f]{2})\\ud[c-f][0-9a-f]{2}/i.test(json);
}

test("cuts never split an emoji, and page strings with a lone surrogate are repaired", () => {
  const rockets = "x🚀".repeat(40);
  // A plain UTF-16 cut leaves half an emoji at the edge.
  expect(noLoneSurrogate(JSON.stringify(rockets.slice(-64)))).toBe(false);
  expect(noLoneSurrogate(JSON.stringify(rockets.slice(0, 2)))).toBe(false);
  for (let n = 1; n <= 12; n++) {
    expect(noLoneSurrogate(JSON.stringify(headUnits(rockets, n)))).toBe(true);
    expect(noLoneSurrogate(JSON.stringify(tailUnits(rockets, n)))).toBe(true);
    expect(headUnits(rockets, n).length).toBeLessThanOrEqual(n);
    expect(tailUnits(rockets, n).length).toBeLessThanOrEqual(n);
  }
  expect(wellFormed("a\ud83db\ude80c🚀")).toBe("a\ufffdb\ufffdc🚀");
  expect(wellFormed("plain 🚀")).toBe("plain 🚀");
});

test("an emoji at every selection boundary still makes a sendable payload", async () => {
  const rockets = "x🚀".repeat(1200);
  const selection = {
    // Each one ends or starts inside an emoji at the client's limits.
    quote: rockets.slice(0, LIMITS.quoteChars + 1),
    prefix: rockets.slice(-(LIMITS.quoteContextChars + 1)),
    suffix: rockets.slice(0, LIMITS.quoteContextChars + 1),
    element: { ...element, text: rockets.slice(0, 601), html: "\ud83d<p>", label: "\ude80" },
    rect: { x: 1, y: 2, width: 3, height: 4 },
  };
  const parsed = parseHelperEvent(msg({ type: "text-selected", id: 9, selection }), NONCE).selection;
  expect(parsed.quote.length).toBeLessThanOrEqual(LIMITS.quoteChars);
  expect(parsed.prefix.length).toBeLessThanOrEqual(LIMITS.quoteContextChars);
  expect(parsed.suffix.length).toBeLessThanOrEqual(LIMITS.quoteContextChars);
  expect(parsed.prefix.endsWith("🚀")).toBe(true);
  expect(parsed.element.html).toBe("\ufffd<p>");

  const payload = await buildPayload({
    submissionId: "sub-12345678",
    artifactPath: "/a/report.html",
    sha256: "a".repeat(64),
    allowStale: false,
    viewport: { width: 390, height: 700, scroll_x: 0, scroll_y: 0 },
    drafts: [
      { id: "t", intent: "change", comment: "ok", target: { kind: "text", ...parsed }, voice: [], images: [] },
      // A draft saved before the fix still holds a lone surrogate.
      {
        id: "e",
        intent: "change",
        comment: "old",
        target: { kind: "element", element: { ...element, text: "half \ud83d" } },
        voice: [],
        images: [],
      },
    ],
  });
  const body = JSON.stringify(payload);
  expect(noLoneSurrogate(body)).toBe(true);
  expect(payload.annotations[1].target.element.text).toBe("half \ufffd");
});
