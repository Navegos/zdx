import { expect, test } from "bun:test";
import { HELPER_NS, TREE_LIMITS, injectHelper, marksFor, parseHelperEvent } from "./src/lib/annotate.ts";

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
