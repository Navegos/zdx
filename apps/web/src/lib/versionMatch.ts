/**
 * Update to a newer version of an open artifact: which notes can move to it.
 *
 * Every check is mechanical and conservative. A note moves only when its
 * target is found in the new version exactly as it was captured; anything
 * uncertain fails with a reason. A text note's box is never carried over:
 * the page measures where the matched words are now (`range-rects`), and a
 * note whose words cannot be placed fails. Passing is not proof that the target still
 * means the same thing (two identical buttons can swap places), which is why
 * moved notes keep `carriedFrom` and the agent is told so.
 *
 * Notes move all together or not at all: a draft record and a submission name
 * one version. Pure functions over bridge answers; no `$lib` imports, so
 * `version-match.test.mjs` can load it under `bun test`.
 */
import { headUnits, tailUnits, wellFormed, LIMITS } from "./annotate";
import type {
  DraftAnnotation,
  DraftTarget,
  InspectedItem,
  MeasuredRange,
  PageElement,
  PageViewport,
  Rect,
} from "./annotate";

export type NoteCheck = { ok: true; target: DraftTarget } | { ok: false; reason: string };

/** Layout drift tolerated when comparing boxes, in CSS px. */
const SLACK = 1;

export interface InspectPlan {
  /** What to ask the new page, in order. */
  items: { selector: string; raw: boolean }[];
  /** Per draft, where its answers start in `items` and how many there are. */
  slots: { start: number; count: number }[];
}

/** The `inspect` request for the new page, built from the notes' own
 *  captured selectors (the same strings marks already send into the page). */
export function inspectPlan(drafts: readonly DraftAnnotation[]): InspectPlan {
  const items: InspectPlan["items"] = [];
  const slots = drafts.map((draft) => {
    const start = items.length;
    const t = draft.target;
    if (t.kind === "element") items.push({ selector: t.element.selector, raw: false });
    else if (t.kind === "text") items.push({ selector: t.element.selector, raw: true });
    else if (t.kind === "drawing") {
      if (t.anchor) items.push({ selector: t.anchor.selector, raw: false });
      for (const el of t.elements) items.push({ selector: el.selector, raw: false });
    }
    return { start, count: items.length - start };
  });
  return { items, slots };
}

function near(a: number, b: number): boolean {
  return Math.abs(a - b) <= SLACK;
}

function sameBox(a: Rect, b: Rect): boolean {
  return near(a.x, b.x) && near(a.y, b.y) && near(a.width, b.width) && near(a.height, b.height);
}

/** Why `item` is not the element `old` was, or null when it is: one match,
 *  same tag, same text, label, and role, and the same markup (as captured,
 *  up to 2000 characters), so a changed image source or style also fails. */
function elementProblem(old: PageElement, item: InspectedItem | undefined): string | null {
  if (!item || item.count === 0 || !item.element) return "not found in the new version";
  if (item.count > 1) return "more than one element matches it now";
  const now = item.element;
  if (now.tag !== old.tag) return "it is a different kind of element now";
  if (now.text !== old.text || now.label !== old.label || now.role !== old.role) return "its text changed";
  if (now.html !== old.html) return "its markup changed";
  return null;
}

const collapse = (text: string) => text.replace(/\s+/g, " ");

/** Offsets of the quote in the holding element's text with the same words
 *  around it, computed the way the page bridge captured them. */
function quoteMatches(raw: string, quote: string, prefix: string, suffix: string): number[] {
  const found: number[] = [];
  for (let at = raw.indexOf(quote); at !== -1; at = raw.indexOf(quote, at + 1)) {
    const before = wellFormed(tailUnits(collapse(raw.slice(0, at)), LIMITS.quoteContextChars));
    const after = wellFormed(headUnits(collapse(raw.slice(at + quote.length)), LIMITS.quoteContextChars));
    if (before === prefix && after === suffix) found.push(at);
  }
  return found;
}

/** The measured boxes by note index. A box counts only when the words the
 *  page measured are exactly the quote: the page may have changed its text
 *  between the `inspect` answer and the measurement. */
export function acceptMeasured(
  drafts: readonly DraftAnnotation[],
  ranges: readonly RangeRequest[],
  measured: readonly (MeasuredRange | null)[] | null,
): Map<number, Rect | null> {
  const out = new Map<number, Rect | null>();
  ranges.forEach((range, i) => {
    const t = drafts[range.note]?.target;
    const m = measured?.[i] ?? null;
    out.set(range.note, m && t?.kind === "text" && m.text === t.quote ? m.rect : null);
  });
  return out;
}

/** A text range to measure in the new page, for one text note. */
export interface RangeRequest {
  /** Index of the note in the drafts. */
  note: number;
  selector: string;
  start: number;
  end: number;
}

/** Where the quote of each text note is in the new page's text, when it is
 *  there exactly once with the same context: the ranges to measure. */
export function rangeRequests(
  drafts: readonly DraftAnnotation[],
  plan: InspectPlan,
  answers: InspectedItem[] | null,
): RangeRequest[] {
  if (!answers) return [];
  return drafts.flatMap((draft, i) => {
    const t = draft.target;
    const item = answers[plan.slots[i].start];
    if (t.kind !== "text" || !item || item.count !== 1 || item.raw === undefined) return [];
    const found = quoteMatches(item.raw, t.quote, t.prefix, t.suffix);
    if (found.length !== 1) return [];
    return [{ note: i, selector: t.element.selector, start: found[0], end: found[0] + t.quote.length }];
  });
}

function checkOne(
  draft: DraftAnnotation,
  answers: InspectedItem[],
  viewport: PageViewport | null,
  measured: Rect | null | undefined,
): NoteCheck {
  const t = draft.target;
  switch (t.kind) {
    case "page":
      return { ok: true, target: t };
    case "image_pin":
    case "image_area":
      return { ok: false, reason: "an image note cannot move to a page" };
    case "element": {
      const problem = elementProblem(t.element, answers[0]);
      if (problem) return { ok: false, reason: problem };
      return { ok: true, target: { kind: "element", element: answers[0].element! } };
    }
    case "text": {
      const item = answers[0];
      if (!item || item.count === 0 || !item.element) return { ok: false, reason: "the text around it was not found" };
      if (item.count > 1) return { ok: false, reason: "more than one element matches the text around it" };
      const now = item.element;
      if (now.tag !== t.element.tag) return { ok: false, reason: "the text around it is in a different element now" };
      if (item.raw === undefined) return { ok: false, reason: "the text around it is too long to check" };
      const found = quoteMatches(item.raw, t.quote, t.prefix, t.suffix);
      if (found.length === 0) return { ok: false, reason: "the selected words or the words around them changed" };
      if (found.length > 1) return { ok: false, reason: "the selected words appear more than once now" };
      // The box comes only from the new page; the old one is never shifted.
      if (!measured || measured.width <= 0 || measured.height <= 0) {
        return { ok: false, reason: "where the selected words are now could not be measured" };
      }
      return { ok: true, target: { ...t, element: now, rect: measured } };
    }
    case "drawing": {
      // The stroke was drawn over a layout; it moves only when every element
      // it covered is still there, unchanged, in the same box, at the same
      // page width. The sketch still shows the earlier layout either way.
      const olds = [...(t.anchor ? [t.anchor] : []), ...t.elements];
      if (olds.length === 0) return { ok: false, reason: "no page elements were under it" };
      // The capture lists at most this many; more may have been under it.
      if (t.elements.length >= LIMITS.drawingElements) {
        return { ok: false, reason: "it covered too many elements to check" };
      }
      if (!viewport || !near(viewport.width, t.viewport.width)) {
        return { ok: false, reason: "the page is shown at a different width now" };
      }
      for (let i = 0; i < olds.length; i++) {
        const problem = elementProblem(olds[i], answers[i]);
        if (problem) return { ok: false, reason: `an element under it: ${problem}` };
        if (!sameBox(olds[i].rect, answers[i].element!.rect)) return { ok: false, reason: "the layout under it changed" };
      }
      return { ok: true, target: t };
    }
  }
}

/** Checks every note of an HTML artifact against the new page's answers.
 *  `answers` null means the new page did not answer: nothing but page notes
 *  can be checked. `measured` holds the boxes the page reported for
 *  `rangeRequests` (by note index); a text note without one fails. */
export function checkHtmlNotes(
  drafts: readonly DraftAnnotation[],
  plan: InspectPlan,
  answers: InspectedItem[] | null,
  viewport: PageViewport | null,
  measured: ReadonlyMap<number, Rect | null> = new Map(),
): NoteCheck[] {
  return drafts.map((draft, i) => {
    if (!answers) {
      return draft.target.kind === "page"
        ? { ok: true, target: draft.target }
        : { ok: false, reason: "the new version did not answer the check" };
    }
    const slot = plan.slots[i];
    return checkOne(draft, answers.slice(slot.start, slot.start + slot.count), viewport, measured.get(i));
  });
}

/** Image notes move only when the server's normalized copy is byte for byte
 *  the same, so every mark lands on the same pixels. */
export function checkImageNotes(drafts: readonly DraftAnnotation[], sameCopy: boolean): NoteCheck[] {
  return drafts.map((draft) => {
    const t = draft.target;
    if (t.kind === "page") return { ok: true, target: t };
    if (sameCopy && (t.kind === "image_pin" || t.kind === "image_area")) return { ok: true, target: t };
    return { ok: false, reason: t.kind === "image_pin" || t.kind === "image_area" ? "the image changed" : "not an image note" };
  });
}

export function allMatched(checks: readonly NoteCheck[]): boolean {
  return checks.every((check) => check.ok);
}

/** The notes on the new version, when every check passed. Each keeps the
 *  first version it was written on. */
export function carryOver(drafts: readonly DraftAnnotation[], checks: readonly NoteCheck[], fromSha: string): DraftAnnotation[] {
  if (checks.length !== drafts.length || !allMatched(checks)) throw new Error("not every note matched");
  return drafts.map((draft, i) => ({
    ...draft,
    target: (checks[i] as { ok: true; target: DraftTarget }).target,
    carriedFrom: draft.carriedFrom ?? fromSha,
  }));
}
