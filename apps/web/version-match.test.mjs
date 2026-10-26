import { expect, test } from "bun:test";
import { HELPER_NS, buildPayload, injectHelper, parseHelperEvent } from "./src/lib/annotate.ts";
import {
  acceptMeasured,
  allMatched,
  carryOver,
  checkHtmlNotes,
  checkImageNotes,
  inspectPlan,
  rangeRequests,
} from "./src/lib/versionMatch.ts";

const OLD = "a".repeat(64);
const NONCE = "0123456789abcdef0123456789abcdef";
const rect = (x, y, width, height) => ({ x, y, width, height });
const el = (selector, text, box = rect(0, 100, 300, 40), extra = {}) => ({
  selector,
  tag: "p",
  text,
  html: `<p>${text}</p>`,
  role: "",
  label: "",
  heading: "",
  rect: box,
  ...extra,
});
const note = (target, extra = {}) => ({ id: crypto.randomUUID(), intent: "change", comment: "c", target, voice: [], images: [], ...extra });
const viewport = { width: 390, height: 700, scroll_x: 0, scroll_y: 0 };

const para = "Latency and cost per provider, from the weekly run.";
const elementNote = note({ kind: "element", element: el("#a", "Pricing") });
const textNote = note({
  kind: "text",
  quote: "cost per provider",
  prefix: "Latency and ",
  suffix: ", from the weekly run.",
  element: el("body > p", para),
  rect: rect(80, 104, 120, 18),
});
const drawingNote = note({
  kind: "drawing",
  bounds: rect(0, 90, 300, 80),
  anchor: el("main", "Pricing Plans", rect(0, 80, 390, 300), { tag: "main" }),
  offset: null,
  elements: [el("#a", "Pricing")],
  strokes: [[[1, 1]]],
  viewport,
  sketch: new Blob(["png"]),
});
const pageNote = note({ kind: "page" });

/** What the new page answers for each planned selector. */
function answers(plan, bySelector) {
  return plan.items.map(({ selector, raw }) => {
    const found = bySelector[selector];
    if (!found) return { count: 0, element: null };
    const { count = 1, element, text } = found;
    return raw ? { count, element, raw: text } : { count, element };
  });
}

const unchanged = {
  "#a": { element: el("#a", "Pricing") },
  "body > p": { element: el("body > p", para, rect(0, 300, 300, 40)), text: para },
  main: { element: el("main", "Pricing Plans", rect(0, 80, 390, 300), { tag: "main" }) },
};

test("unchanged targets move together, keep where they were written, and follow their element", () => {
  const drafts = [elementNote, textNote, drawingNote, pageNote];
  const plan = inspectPlan(drafts);
  expect(plan.items.map((i) => [i.selector, i.raw])).toEqual([
    ["#a", false],
    ["body > p", true],
    ["main", false],
    ["#a", false],
  ]);
  const page = answers(plan, unchanged);
  // The text note's words are measured in the new page, where they are now.
  const ranges = rangeRequests(drafts, plan, page);
  expect(ranges).toEqual([{ note: 1, selector: "body > p", start: 12, end: 29 }]);
  const measured = new Map([[1, rect(80, 304, 120, 18)]]);
  const checks = checkHtmlNotes(drafts, plan, page, viewport, measured);
  expect(checks.map((c) => c.ok)).toEqual([true, true, true, true]);
  expect(allMatched(checks)).toBe(true);
  const moved = carryOver(drafts, checks, OLD);
  expect(moved.every((d) => d.carriedFrom === OLD)).toBe(true);
  expect(moved[1].target.rect).toEqual(rect(80, 304, 120, 18));
  expect(moved[1].target.element.rect.y).toBe(300);
  // A note carried twice keeps the first version.
  const again = carryOver([{ ...moved[3] }], [{ ok: true, target: pageNote.target }], "b".repeat(64));
  expect(again[0].carriedFrom).toBe(OLD);
});

test("anything uncertain fails with a reason and nothing moves", () => {
  const cases = [
    [elementNote, { ...unchanged, "#a": { element: el("#a", "Pricing!") } }, "its text changed"],
    [elementNote, { ...unchanged, "#a": { count: 2, element: el("#a", "Pricing") } }, "more than one element matches it now"],
    [elementNote, { ...unchanged, "#a": undefined }, "not found in the new version"],
    [elementNote, { ...unchanged, "#a": { element: el("#a", "Pricing", undefined, { tag: "div" }) } }, "different kind"],
    [textNote, { ...unchanged, "body > p": { element: el("body > p", para), text: "Latency and cost per region, from the weekly run." } }, "words around them changed"],
    [textNote, { ...unchanged, "body > p": { element: el("body > p", para), text: `Latency and cost per provider, from the weekly run. ${para}` } }, "words around them changed"],
    [textNote, { ...unchanged, "body > p": { element: el("body > p", para), text: para } }, "could not be measured"],
    [textNote, { ...unchanged, "body > p": { element: el("body > p", para), text: undefined } }, "too long"],
    [drawingNote, { ...unchanged, "#a": { element: el("#a", "Pricing", rect(0, 110, 300, 40)) } }, "layout under it changed"],
    // Same text, label, and role, but the markup changed (a new image source, a style).
    [elementNote, { ...unchanged, "#a": { element: el("#a", "Pricing", undefined, { html: '<p class="new">Pricing</p>' }) } }, "markup changed"],
    [drawingNote, { ...unchanged, "#a": { element: el("#a", "Pricing", undefined, { html: "<p>other</p>" }) } }, "markup changed"],
  ];
  for (const [draft, page, reason] of cases) {
    const drafts = [draft, pageNote];
    const plan = inspectPlan(drafts);
    const checks = checkHtmlNotes(drafts, plan, answers(plan, page), viewport);
    expect(checks[0].ok).toBe(false);
    expect(checks[0].reason).toContain(reason);
    expect(allMatched(checks)).toBe(false);
    expect(() => carryOver(drafts, checks, OLD)).toThrow();
  }
});

test("a quote matching twice, a different page width, or a page that does not answer all fail", () => {
  // Context cut at 64 characters can repeat: two places with the same words around them.
  const pad = "z".repeat(70);
  const repeated = note({
    ...textNote.target,
    quote: "cost",
    prefix: `${pad} `.slice(-64),
    suffix: ` ${pad}`.slice(0, 64),
    element: el("body > p", ""),
  });
  const raw = `${pad} cost ${pad} cost ${pad}`;
  const twicePlan = inspectPlan([repeated]);
  const twice = checkHtmlNotes([repeated], twicePlan, [{ count: 1, element: el("body > p", ""), raw }], viewport);
  expect(twice[0].reason).toContain("more than once");

  const drawPlan = inspectPlan([drawingNote]);
  const narrow = checkHtmlNotes([drawingNote], drawPlan, answers(drawPlan, unchanged), { ...viewport, width: 360 });
  expect(narrow[0].reason).toContain("different width");

  const silent = checkHtmlNotes([elementNote, pageNote], inspectPlan([elementNote, pageNote]), null, null);
  expect(silent.map((c) => c.ok)).toEqual([false, true]);
  expect(silent[0].reason).toContain("did not answer");
});

test("image notes move only with a byte-identical copy", () => {
  const pin = note({ kind: "image_pin", point: { x: 0.5, y: 0.5 } });
  const area = note({ kind: "image_area", strokes: [[{ x: 0.1, y: 0.1 }]] });
  expect(checkImageNotes([pin, area, pageNote], true).every((c) => c.ok)).toBe(true);
  const changed = checkImageNotes([pin, area, pageNote], false);
  expect(changed.map((c) => c.ok)).toEqual([false, false, true]);
  expect(changed[0].reason).toBe("the image changed");
});

test("a carried note says so in the payload; others do not", async () => {
  const payload = await buildPayload({
    submissionId: "sub-12345678",
    artifactPath: "/a/report.html",
    sha256: "c".repeat(64),
    allowStale: false,
    viewport,
    drafts: [{ ...pageNote, carriedFrom: OLD }, elementNote],
  });
  expect(payload.annotations[0].carried_from).toBe(OLD);
  expect("carried_from" in payload.annotations[1]).toBe(false);
});

test("inspect answers from the page are bounded and checked like any report", () => {
  const msg = (extra) => ({ [HELPER_NS]: 1, nonce: NONCE, ...extra });
  const items = [
    { count: 1, element: el("#a", "x"), raw: "text" },
    { count: -3, element: { selector: "" } },
    { count: 1, element: el("#b", "y"), raw: "z".repeat(200_001) },
    { count: 1, element: el("#c", "y"), truncated: true },
    "junk",
  ];
  const parsed = parseHelperEvent(msg({ type: "inspected", id: 5, items, viewport }), NONCE);
  expect(parsed.type).toBe("inspected");
  expect(parsed.items[0]).toEqual({ count: 1, element: el("#a", "x"), raw: "text" });
  expect(parsed.items[1]).toEqual({ count: 0, element: null });
  expect(parsed.items[2].truncated).toBe(true);
  expect(parsed.items[2].raw).toBeUndefined();
  expect(parsed.items[3].truncated).toBe(true);
  expect(parsed.items[4]).toEqual({ count: 0, element: null });
  expect(parseHelperEvent(msg({ type: "inspected", items }), NONCE)).toBeNull();
  // The bridge answers inspect requests; it never sends one on its own.
  expect(injectHelper("<p>x</p>", NONCE)).toContain('d.type === "inspect"');
});

/** The text-note cases from review: the box always comes from the new page. */
test("a text note gets the box where its words are now, or does not move", () => {
  const box = rect(8, 100, 300, 60);
  const oldRaw = `Short intro. ${"filler words here ".repeat(5)}the KEY phrase sits here and more words follow after it.`;
  const at = oldRaw.indexOf("KEY phrase");
  const ctx = (raw, i) => ({
    prefix: raw.slice(0, i).replace(/\s+/g, " ").slice(-64),
    suffix: raw.slice(i + 10).replace(/\s+/g, " ").slice(0, 64),
  });
  const keyNote = note({
    kind: "text",
    quote: "KEY phrase",
    ...ctx(oldRaw, at),
    element: el("#p", oldRaw, box),
    rect: rect(120, 130, 70, 16),
  });
  const plan = inspectPlan([keyNote]);

  // 1. The opening sentence changed outside the 64-character context: same
  //    paragraph box, the words moved inside it.
  const newRaw = `A much longer introduction that pushes every later word along the lines. ${oldRaw.slice(13)}`;
  const page = [{ count: 1, element: el("#p", newRaw, box), raw: newRaw }];
  const [range] = rangeRequests([keyNote], plan, page);
  expect(range.start).toBe(newRaw.indexOf("KEY phrase"));
  expect(range.start).not.toBe(at);
  const moved = checkHtmlNotes([keyNote], plan, page, viewport, new Map([[0, rect(40, 146, 70, 16)]]));
  expect(moved[0].ok).toBe(true);
  expect(moved[0].target.rect).toEqual(rect(40, 146, 70, 16));
  // Never the old box shifted, and never without a measurement.
  for (const measured of [new Map(), new Map([[0, null]]), new Map([[0, rect(40, 146, 0, 0)]])]) {
    const unplaced = checkHtmlNotes([keyNote], plan, page, viewport, measured);
    expect(unplaced[0].ok).toBe(false);
    expect(unplaced[0].reason).toContain("could not be measured");
  }

  // 2. A container longer than the 600 characters captured as element text:
  //    matched on its full text, at the offset past the cut.
  const longRaw = `${"lorem ipsum ".repeat(80)}the KEY phrase sits here and more words follow after it.`;
  const longNote = note({ ...keyNote.target, ...ctx(longRaw, longRaw.indexOf("KEY phrase")), element: el("#p", longRaw.slice(0, 600), box) });
  const longPlan = inspectPlan([longNote]);
  const longPage = [{ count: 1, element: el("#p", longRaw.slice(0, 600), box), raw: longRaw }];
  const [longRange] = rangeRequests([longNote], longPlan, longPage);
  expect(longRange.start).toBe(longRaw.indexOf("KEY phrase"));
  expect(longRange.start).toBeGreaterThan(600);
  expect(checkHtmlNotes([longNote], longPlan, longPage, viewport, new Map([[0, rect(9, 400, 70, 16)]]))[0].target.rect).toEqual(
    rect(9, 400, 70, 16),
  );

  // 3. Same outer box and same text, but the words were laid out elsewhere
  //    (a font or CSS change): the measured box wins.
  const samePage = [{ count: 1, element: el("#p", oldRaw, box), raw: oldRaw }];
  const reflowed = checkHtmlNotes([keyNote], plan, samePage, viewport, new Map([[0, rect(200, 114, 70, 16)]]));
  expect(reflowed[0].target.rect).toEqual(rect(200, 114, 70, 16));

  // Nothing to measure when the words are not there exactly once.
  expect(rangeRequests([keyNote], plan, [{ count: 1, element: el("#p", "x", box), raw: "nothing here" }])).toEqual([]);
  expect(rangeRequests([keyNote], plan, null)).toEqual([]);
});

test("a drawing that covered the most elements the capture lists does not move", () => {
  const many = Array.from({ length: 12 }, (_, i) => el(`#e${i}`, `E${i}`));
  const full = note({ ...drawingNote.target, anchor: null, elements: many });
  const plan = inspectPlan([full]);
  const page = plan.items.map(({ selector }) => ({ count: 1, element: many.find((e) => e.selector === selector) }));
  const [check] = checkHtmlNotes([full], plan, page, viewport);
  expect(check.ok).toBe(false);
  expect(check.reason).toContain("too many elements");
});

test("range boxes from the page are checked like any report", () => {
  const msg = (extra) => ({ [HELPER_NS]: 1, nonce: NONCE, ...extra });
  const parsed = parseHelperEvent(
    msg({
      type: "range-rects",
      id: 7,
      rects: [
        { ...rect(1, 2, 3, 4), text: "KEY phrase" },
        null,
        { x: "1", text: "a" },
        { ...rect(0, 0, -1, 2), text: "a" },
        rect(1, 2, 3, 4),
        { ...rect(1, 2, 3, 4), text: "z".repeat(2001) },
        { ...rect(1, 2, 3, 4), text: "half \ud83d" },
      ],
    }),
    NONCE,
  );
  expect(parsed.rects).toEqual([
    { rect: rect(1, 2, 3, 4), text: "KEY phrase" },
    null,
    null,
    null,
    null,
    null,
    { rect: rect(1, 2, 3, 4), text: "half \ufffd" },
  ]);
  expect(parseHelperEvent(msg({ type: "range-rects", rects: [] }), NONCE)).toBeNull();
  expect(injectHelper("<p>x</p>", NONCE)).toContain('d.type === "range-rects"');
});

test("a measured box counts only when the page measured the quote itself", () => {
  const drafts = [textNote, pageNote, { ...textNote, id: "other" }];
  const ranges = [
    { note: 0, selector: "body > p", start: 12, end: 29 },
    { note: 2, selector: "body > p", start: 12, end: 29 },
  ];
  const accepted = acceptMeasured(drafts, ranges, [
    { rect: rect(80, 304, 120, 18), text: "cost per provider" },
    // The page changed its text between the inspect answer and the measurement.
    { rect: rect(80, 304, 120, 18), text: "t per provider, f" },
  ]);
  expect(accepted.get(0)).toEqual(rect(80, 304, 120, 18));
  expect(accepted.get(2)).toBeNull();
  expect([...acceptMeasured(drafts, ranges, null).values()]).toEqual([null, null]);
  const plan = inspectPlan([textNote]);
  const [check] = checkHtmlNotes([textNote], plan, answers(plan, unchanged), viewport, new Map([[0, accepted.get(2)]]));
  expect(check.reason).toContain("could not be measured");
});

test("the checker is ready only on the settle answer sent after load", () => {
  const msg = (extra) => ({ [HELPER_NS]: 1, nonce: NONCE, ...extra });
  expect(parseHelperEvent(msg({ type: "settled", id: 3, viewport }), NONCE)).toEqual({ type: "settled", id: 3, viewport });
  expect(parseHelperEvent(msg({ type: "settled" }), NONCE)).toBeNull();
  const bridge = injectHelper("<p>x</p>", NONCE);
  // The bridge waits for the page's fonts before answering a settle.
  expect(bridge).toContain('d.type === "settle"');
  expect(bridge).toContain("doc.fonts && doc.fonts.ready");
});
