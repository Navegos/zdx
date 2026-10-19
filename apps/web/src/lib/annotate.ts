/**
 * Artifact annotation: the in-page bridge, its message protocol, and payload
 * building for `POST /api/threads/{id}/artifacts/feedback`.
 *
 * The HTML preview runs in an opaque-origin sandbox (`allow-scripts` only), so
 * the app cannot read the page DOM or pixels. A small bridge script is
 * injected into a copy of the page instead and answers `postMessage` requests.
 *
 * Trust rules:
 * - Everything the frame sends is **untrusted**: the page's own scripts share
 *   its window and can patch the DOM methods the bridge uses, so element data
 *   may be forged. Messages are schema-checked, numbers bounded, strings
 *   capped. Each carries a per-load nonce; it filters stray messages from
 *   other loads and pages, but it is not an authentication: do not treat a
 *   report as trusted because its nonce matches. The bridge removes its own
 *   script and builds messages without page-patchable helpers, which keeps the
 *   nonce out of easy reach, nothing more.
 * - The frame never starts anything. It only answers requests the parent made
 *   (with an id the parent is waiting for). A pick becomes a draft the user
 *   sees and edits; nothing reaches the agent without the Send tap.
 * - The parent sends the frame geometry, ids, and mode only — never comments,
 *   media, thread ids, or credentials: page scripts can read anything sent in.
 *
 * Kept free of `$lib` imports so `bun test` can load it directly.
 */

import type { ImagePoint } from "./imageMarks";

export const HELPER_NS = "zdx-annotate";
export const HELPER_VERSION = 1;

export type FeedbackIntent = "change" | "keep" | "question" | "note";

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface PageViewport {
  width: number;
  height: number;
  scroll_x: number;
  scroll_y: number;
  page_width?: number;
  page_height?: number;
}

export interface PageElement {
  selector: string;
  tag: string;
  text: string;
  html: string;
  role: string;
  label: string;
  heading: string;
  /** Page (document) coordinates, CSS px, at the time of capture. */
  rect: Rect;
}

export type Point = [number, number];

export interface DrawingTarget {
  kind: "drawing";
  /** Page coordinates. */
  bounds: Rect;
  /** Smallest element holding the whole stroke; the mark follows it. */
  anchor: PageElement | null;
  /** Stroke bounds relative to the anchor's box, when there is an anchor. */
  offset: Rect | null;
  elements: PageElement[];
  /** Strokes in viewport coordinates at the time of drawing. */
  strokes: Point[][];
  viewport: PageViewport;
  sketch: Blob;
}

/** A pin on an image artifact, in fractions of the displayed image. */
export interface ImagePinTarget {
  kind: "image_pin";
  point: ImagePoint;
}

/** Finger strokes on an image artifact, in fractions of the displayed image. */
export interface ImageAreaTarget {
  kind: "image_area";
  strokes: ImagePoint[][];
}

/** The canonical copy of an image artifact, as the server named it. */
export interface ImageFrame {
  canonical_sha256: string;
  width: number;
  height: number;
}

export type DraftTarget =
  | { kind: "element"; element: PageElement }
  | DrawingTarget
  | ImagePinTarget
  | ImageAreaTarget
  | { kind: "page" };

export interface VoiceClip {
  blob: Blob;
  durationMs: number;
}

export interface ImageClip {
  blob: Blob;
}

export interface DraftAnnotation {
  id: string;
  intent: FeedbackIntent;
  comment: string;
  target: DraftTarget;
  voice: VoiceClip[];
  images: ImageClip[];
}

export const INTENTS: { value: FeedbackIntent; label: string }[] = [
  { value: "change", label: "Change" },
  { value: "keep", label: "Keep" },
  { value: "question", label: "Question" },
  { value: "note", label: "Note" },
];

/** Mirrors `zdx_engine::core::artifact_feedback` limits. */
export const LIMITS = {
  annotations: 20,
  commentChars: 4000,
  mediaPerAnnotation: 4,
  imageBytes: 8 * 1024 * 1024,
  audioBytes: 10 * 1024 * 1024,
  drawingElements: 12,
  voiceMs: 3 * 60 * 1000,
};

/** Elements that draw pixels the bridge cannot capture. */
const PIXEL_TAGS = new Set(["canvas", "svg", "img", "video", "iframe", "picture"]);

export function targetDrawsPixels(target: DraftTarget): boolean {
  if (target.kind === "element") return PIXEL_TAGS.has(target.element.tag);
  if (target.kind === "drawing") return target.elements.some((e) => PIXEL_TAGS.has(e.tag));
  return false;
}

/**
 * Runs inside the artifact page, before any page script. `__NONCE__` is
 * replaced per load. It removes its own `<script>` element and keeps no
 * globals, keeps private copies of the DOM methods it relies on, and only ever
 * posts to `window.parent` (never `top`: in Telegram Web the Mini App itself is
 * framed). It does not intercept input: the parent covers the frame while the
 * user taps or draws, so links and page handlers never fire.
 */
const HELPER_SOURCE = String.raw`(() => {
  const NS = "zdx-annotate", V = 1, NONCE = "__NONCE__";
  const self = document.currentScript;
  if (self) self.remove();
  const doc = document, win = window, P = win.parent;
  const rectOf = Element.prototype.getBoundingClientRect;
  const fromPoint = Document.prototype.elementFromPoint;
  const contains = Node.prototype.contains;
  const qs = Document.prototype.querySelector;
  const qsa = Document.prototype.querySelectorAll;
  const style = win.getComputedStyle.bind(win);
  const raf = win.requestAnimationFrame.bind(win);
  // Prototype getters captured before page scripts run: a page can redefine
  // parentElement/children on its own elements (even into cycles), and the
  // tree must describe the real DOM in bounded steps.
  const getter = (proto, name) => Object.getOwnPropertyDescriptor(proto, name).get;
  const parentOf = getter(Node.prototype, "parentElement");
  const firstChildOf = getter(Element.prototype, "firstElementChild");
  const nextOf = getter(Element.prototype, "nextElementSibling");
  const childCountOf = getter(Element.prototype, "childElementCount");
  const connected = getter(Node.prototype, "isConnected");
  // Object spread, not Object.assign: a page script could replace the latter
  // and read every message, nonce included. This narrows the easy leaks; it
  // does not make the nonce a secret the page provably cannot reach.
  const post = (msg) => { try { P.postMessage({ ...msg, [NS]: V, nonce: NONCE }, "*"); } catch (_) {} };
  const scroller = () => doc.scrollingElement || doc.documentElement;
  const viewport = () => {
    const s = scroller();
    return { width: win.innerWidth, height: win.innerHeight, scroll_x: win.scrollX, scroll_y: win.scrollY,
      page_width: s ? s.scrollWidth : win.innerWidth, page_height: s ? s.scrollHeight : win.innerHeight };
  };
  const esc = (v) => (win.CSS && CSS.escape ? CSS.escape(v) : String(v).replace(/[^a-zA-Z0-9_-]/g, "\\$&"));
  let host = null, root = null, boxes = [], marks = [], lastUnresolved = "", running = false;
  // Elements the parent can refer to by number (selection tree). Rebuilt
  // before it grows large; the parent only uses ids from the latest answer.
  let nodes = [], nodeIds = new Map(), selected = null, selBox = null, shades = [], lastState = null;
  const own = (el) => host && (el === host || contains.call(host, el));
  function selectorFor(el) {
    const parts = [];
    let node = el;
    while (node && node.nodeType === 1 && parts.length < 10) {
      const tag = node.tagName.toLowerCase();
      if (node.id) {
        let unique = false;
        try { unique = qsa.call(doc, "#" + esc(node.id)).length === 1; } catch (_) {}
        if (unique) { parts.unshift("#" + esc(node.id)); break; }
      }
      if (tag === "html" || tag === "body") { parts.unshift(tag); break; }
      let index = 1, sib = node, same = 0;
      while ((sib = sib.previousElementSibling)) if (sib.tagName === node.tagName) index++;
      if (node.parentElement) for (const c of node.parentElement.children) if (c.tagName === node.tagName) same++;
      parts.unshift(same > 1 ? tag + ":nth-of-type(" + index + ")" : tag);
      node = node.parentElement;
    }
    return parts.join(" > ");
  }
  function headingFor(el) {
    let found = "";
    let list = [];
    try { list = qsa.call(doc, "h1,h2,h3,h4,h5,h6"); } catch (_) {}
    for (const h of list) {
      if (h === el || contains.call(h, el)) continue;
      if (h.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) found = h.textContent || "";
      else break;
    }
    return found.replace(/\s+/g, " ").trim().slice(0, 200);
  }
  function describe(el) {
    const r = rectOf.call(el);
    const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim().slice(0, 600);
    let html = "";
    try { html = el.outerHTML.slice(0, 2000); } catch (_) {}
    const attr = (n) => (el.getAttribute && el.getAttribute(n)) || "";
    return { selector: selectorFor(el).slice(0, 512), tag: el.tagName.toLowerCase().slice(0, 32), text, html,
      role: attr("role").slice(0, 200), label: (attr("aria-label") || attr("title") || attr("alt")).slice(0, 200),
      heading: headingFor(el), rect: { x: r.left + win.scrollX, y: r.top + win.scrollY, width: r.width, height: r.height } };
  }
  function nodeId(el) {
    let id = nodeIds.get(el);
    if (id === undefined) { id = nodes.length; nodes.push(el); nodeIds.set(el, id); }
    return id;
  }
  const blocked = (el) => !el || el.nodeType !== 1 || own(el) || /^(html|body|head|script|style)$/i.test(el.tagName);
  function brief(el) {
    const cls = typeof el.className === "string" ? el.className.trim().split(/\s+/)[0] || "" : "";
    const label = el.id ? "#" + el.id : cls ? "." + cls : "";
    // First words of the text, with a space between text nodes; stops early
    // so a large container costs no more than a small one.
    let text = "";
    const walker = doc.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let t = walker.nextNode(); t && text.length < 48; t = walker.nextNode()) {
      const part = (t.nodeValue || "").replace(/\s+/g, " ").trim();
      if (part && !/^(script|style)$/i.test(t.parentNode ? t.parentNode.nodeName : "")) text += (text ? " " : "") + part;
    }
    text = text.slice(0, 48);
    return { node: nodeId(el), tag: el.tagName.toLowerCase().slice(0, 32), label: label.slice(0, 48), text,
      children: childCountOf.call(el) };
  }
  // A local tree: the ancestors up to <body> (nearest 8) and the first 12
  // element children. Never the whole DOM.
  function selection(el) {
    if (nodes.length > 300) { nodes = []; nodeIds = new Map(); }
    const ancestors = [];
    let p = parentOf.call(el);
    while (p && !blocked(p) && ancestors.length < 8) { ancestors.unshift(brief(p)); p = parentOf.call(p); }
    const kids = [];
    let count = 0;
    for (let c = firstChildOf.call(el), seen = 0; c && seen < 200; c = nextOf.call(c), seen++) {
      if (blocked(c)) continue;
      count++;
      if (kids.length < 12) kids.push(c);
    }
    return { node: nodeId(el), element: describe(el),
      tree: { ancestors, self: brief(el), children: kids.map(brief), child_count: count } };
  }
  // Brings an element into view by scrolling only this page and its own
  // scroll boxes. Not scrollIntoView: that can also scroll the app around the
  // frame.
  function reveal(el) {
    // 0 when the element is fully in view, or taller than the view and already
    // covering part of it; otherwise the smallest scroll that shows it.
    const fit = (er, top, bottom) => {
      const tall = er.bottom - er.top > bottom - top - 16;
      if (tall) return er.bottom > top + 24 && er.top < bottom - 24 ? 0 : er.top - top - 8;
      if (er.top < top) return er.top - top - 8;
      if (er.bottom > bottom) return er.bottom - bottom + 8;
      return 0;
    };
    for (let p = parentOf.call(el), n = 0; p && p !== doc.body && p !== doc.documentElement && n < 64; p = parentOf.call(p), n++) {
      const s = style(p);
      if (/(auto|scroll|overlay)/.test(s.overflowY) && p.scrollHeight > p.clientHeight) {
        const pr = rectOf.call(p);
        p.scrollTop += fit(rectOf.call(el), pr.top, pr.top + p.clientHeight);
      }
    }
    const dy = fit(rectOf.call(el), 0, win.innerHeight);
    if (dy) win.scrollBy(0, dy);
  }
  function at(x, y) {
    const el = fromPoint.call(doc, x, y);
    if (!el || own(el)) return null;
    return el.nodeType === 1 ? el : el.parentElement;
  }
  function probe(points) {
    const found = [], seen = new Set();
    for (const p of points.slice(0, 96)) {
      const el = at(+p[0], +p[1]);
      if (!el || seen.has(el)) continue;
      const tag = el.tagName.toLowerCase();
      if (tag === "html" || tag === "body") continue;
      seen.add(el); found.push(el);
    }
    const leaves = found.filter((el) => !found.some((o) => o !== el && contains.call(el, o)));
    let anchor = found[0] || null;
    while (anchor && !found.every((el) => contains.call(anchor, el))) anchor = anchor.parentElement;
    if (anchor && /^(html|body)$/i.test(anchor.tagName)) anchor = null;
    return { anchor: anchor ? describe(anchor) : null, elements: leaves.slice(0, 12).map(describe) };
  }
  function scrollAt(x, y, dx, dy) {
    let el = at(x, y);
    while (el && el !== doc.documentElement && el !== doc.body) {
      const s = style(el);
      const canY = dy && /(auto|scroll|overlay)/.test(s.overflowY) && el.scrollHeight > el.clientHeight;
      const canX = dx && /(auto|scroll|overlay)/.test(s.overflowX) && el.scrollWidth > el.clientWidth;
      if (canX || canY) {
        const top = el.scrollTop, left = el.scrollLeft;
        el.scrollBy(dx, dy);
        if (el.scrollTop !== top || el.scrollLeft !== left) return;
      }
      el = el.parentElement;
    }
    win.scrollBy(dx, dy);
  }
  function ensureLayer() {
    if (host && host.isConnected) return;
    host = doc.createElement("zdx-annotate-marks");
    const hs = host.style;
    hs.position = "fixed"; hs.left = "0"; hs.top = "0"; hs.width = "0"; hs.height = "0";
    hs.pointerEvents = "none"; hs.zIndex = "2147483647"; hs.display = "block";
    root = host.attachShadow({ mode: "closed" });
    boxes = [];
    doc.documentElement.appendChild(host);
  }
  function box(i) {
    if (boxes[i]) return boxes[i];
    const b = doc.createElement("div"), t = doc.createElement("span");
    const bs = b.style, ts = t.style;
    bs.position = "fixed"; bs.boxSizing = "border-box"; bs.pointerEvents = "none"; bs.borderRadius = "4px";
    bs.border = "2px solid #f34e3f"; bs.background = "rgba(243,78,63,0.08)";
    ts.position = "absolute"; ts.left = "-2px"; ts.top = "-20px"; ts.minWidth = "18px"; ts.height = "18px";
    ts.padding = "0 5px"; ts.borderRadius = "9px"; ts.background = "#f34e3f"; ts.color = "#fff";
    ts.font = "600 11px/18px system-ui,sans-serif"; ts.textAlign = "center";
    b.appendChild(t); root.appendChild(b);
    boxes[i] = b;
    return b;
  }
  function find(m) {
    if (m.el && m.el.isConnected) return m.el;
    m.el = null;
    if (typeof m.selector === "string" && m.selector) { try { m.el = qs.call(doc, m.selector); } catch (_) {} }
    return m.el;
  }
  function fixedDiv() {
    const d = doc.createElement("div");
    const s = d.style;
    s.position = "fixed"; s.boxSizing = "border-box"; s.pointerEvents = "none"; s.display = "none";
    return d;
  }
  function selectionBox() {
    if (selBox && root && selBox.isConnected) return selBox;
    // Four shades around the element plus its outline. Shades are computed
    // from the element's box clipped to the viewport, so the whole view is
    // dimmed even when the element is large or off screen.
    shades = [fixedDiv(), fixedDiv(), fixedDiv(), fixedDiv()];
    for (const d of shades) d.style.background = "rgba(15,20,15,0.45)";
    selBox = fixedDiv();
    const s = selBox.style;
    s.borderRadius = "4px"; s.border = "2px solid #286bbd";
    // First children, so numbered marks stay above the dimming.
    for (const d of [selBox, ...shades].reverse()) root.insertBefore(d, root.firstChild);
    return selBox;
  }
  const place = (d, x, y, w, h) => {
    const s = d.style;
    if (w <= 0 || h <= 0) { s.display = "none"; return; }
    s.display = "block"; s.left = x + "px"; s.top = y + "px"; s.width = w + "px"; s.height = h + "px";
  };
  function hideSelection() {
    if (selBox) selBox.style.display = "none";
    for (const d of shades) d.style.display = "none";
  }
  function drawSelection() {
    if (!selected) { hideSelection(); return; }
    ensureLayer();
    const b = selectionBox();
    const gone = !connected.call(selected);
    const er = gone ? null : rectOf.call(selected);
    // "hidden": still in the page but without a box of its own
    // (display: none or display: contents, or zero size).
    const state = gone ? "gone" : er.width === 0 && er.height === 0 ? "hidden" : "ok";
    if (state !== lastState) { lastState = state; post({ type: "selection-status", node: nodeIds.get(selected) ?? -1, state }); }
    const W = win.innerWidth, H = win.innerHeight;
    if (state !== "ok") { hideSelection(); place(shades[0], 0, 0, W, H); return; }
    const l = Math.min(Math.max(er.left, 0), W), r = Math.min(Math.max(er.right, 0), W);
    const t = Math.min(Math.max(er.top, 0), H), bt = Math.min(Math.max(er.bottom, 0), H);
    place(shades[0], 0, 0, W, t);
    place(shades[1], 0, bt, W, H - bt);
    place(shades[2], 0, t, l, bt - t);
    place(shades[3], r, t, W - r, bt - t);
    place(b, er.left - 2, er.top - 2, Math.max(er.width + 4, 6), Math.max(er.height + 4, 6));
  }
  function frame() {
    if (selected) drawSelection();
    else hideSelection();
    if (!marks.length) {
      for (const b of boxes) b.style.display = "none";
      if (!selected) { running = false; return; }
      raf(frame);
      return;
    }
    ensureLayer();
    const unresolved = [];
    marks.forEach((m, i) => {
      let r = null;
      if (m.selector) {
        const el = find(m);
        const er = el ? rectOf.call(el) : null;
        if (er && (er.width > 0 || er.height > 0)) {
          r = m.offset ? { x: er.left + m.offset.x, y: er.top + m.offset.y, width: m.offset.width, height: m.offset.height }
            : { x: er.left, y: er.top, width: er.width, height: er.height };
        }
      } else if (m.rect) {
        r = { x: m.rect.x - win.scrollX, y: m.rect.y - win.scrollY, width: m.rect.width, height: m.rect.height };
      }
      const b = box(i);
      if (!r) { b.style.display = "none"; unresolved.push(m.n); return; }
      const s = b.style;
      s.display = "block"; s.left = r.x + "px"; s.top = r.y + "px";
      s.width = Math.max(r.width, 4) + "px"; s.height = Math.max(r.height, 4) + "px";
      s.borderStyle = m.offset || m.rect ? "dashed" : "solid";
      b.firstChild.textContent = String(m.n);
    });
    for (let i = marks.length; i < boxes.length; i++) boxes[i].style.display = "none";
    const key = unresolved.join(",");
    if (key !== lastUnresolved) { lastUnresolved = key; post({ type: "marks-status", unresolved }); }
    raf(frame);
  }
  const num = (v) => typeof v === "number" && isFinite(v);
  const rectIn = (r) => r && num(r.x) && num(r.y) && num(r.width) && num(r.height) ? { x: r.x, y: r.y, width: r.width, height: r.height } : null;
  // The frame shrinks when the app opens the selection sheet or the note
  // editor (and on phones when the keyboard opens), which can push the
  // selected element out of view. Bring it back once the size settles.
  let resizeQueued = false;
  win.addEventListener("resize", () => {
    if (!selected || resizeQueued) return;
    resizeQueued = true;
    raf(() => raf(() => {
      resizeQueued = false;
      if (selected && connected.call(selected)) { try { reveal(selected); } catch (_) {} }
    }));
  });
  win.addEventListener("message", (e) => {
    if (e.source !== P) return;
    const d = e.data;
    if (!d || d[NS] !== V) return;
    if (d.type === "ping") post({ type: "ready", viewport: viewport() });
    else if (d.type === "pick" && num(d.x) && num(d.y)) {
      const el = at(d.x, d.y);
      post({ type: "picked", id: d.id, viewport: viewport(), ...(blocked(el) ? { element: null } : selection(el)) });
    } else if (d.type === "select" && Number.isInteger(d.node)) {
      const el = nodes[d.node];
      post({ type: "picked", id: d.id, viewport: viewport(), ...(blocked(el) || !connected.call(el) ? { element: null } : selection(el)) });
    } else if (d.type === "highlight") {
      const el = Number.isInteger(d.node) ? nodes[d.node] : null;
      selected = blocked(el) ? null : el;
      lastState = null;
      if (selected && d.reveal === true) { try { reveal(selected); } catch (_) {} }
      if (!running) { running = true; raf(frame); }
    } else if (d.type === "probe" && Array.isArray(d.points)) {
      post({ type: "probed", id: d.id, viewport: viewport(), ...probe(d.points) });
    } else if (d.type === "scroll" && num(d.x) && num(d.y) && num(d.dx) && num(d.dy)) {
      scrollAt(d.x, d.y, d.dx, d.dy);
    } else if (d.type === "marks" && Array.isArray(d.marks)) {
      marks = d.marks.slice(0, 40).map((m) => ({ n: m.n | 0, selector: typeof m.selector === "string" ? m.selector : "",
        offset: rectIn(m.offset), rect: rectIn(m.rect), el: null }));
      lastUnresolved = "-";
      if (!running) { running = true; raf(frame); }
    }
  });
  post({ type: "ready", viewport: viewport() });
})();`;

export function newNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Puts the bridge right after the doctype (skipping a BOM, whitespace, and
 * comments before it), or at the very start when there is no doctype. Never
 * before the doctype: anything but comments there switches the page to quirks
 * mode. Early enough to run before any page script or `<meta>` CSP. Serve the
 * result as `text/html;charset=utf-8`: the script pushes a `<meta charset>`
 * past the parser's 1024-byte scan.
 */
export function injectHelper(html: string, nonce: string): string {
  if (!/^[0-9a-f]{16,64}$/.test(nonce)) throw new Error("invalid nonce");
  const tag = `<script>${HELPER_SOURCE.replace("__NONCE__", () => nonce)}</script>`;
  const doctype = /^\uFEFF?(?:\s|<!--[\s\S]*?-->)*<!doctype[^>]*>/i.exec(html);
  const at = doctype ? doctype[0].length : 0;
  return html.slice(0, at) + tag + html.slice(at);
}

export function helperMessage(msg: Record<string, unknown>): Record<string, unknown> {
  return { [HELPER_NS]: HELPER_VERSION, ...msg };
}

// --- validation of untrusted bridge messages ---

function num(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && Math.abs(value) <= 1e7 ? value : null;
}

function str(value: unknown, max: number): string {
  return typeof value === "string" ? value.slice(0, max) : "";
}

export function parseRect(value: unknown): Rect | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  const x = num(v.x);
  const y = num(v.y);
  const width = num(v.width);
  const height = num(v.height);
  if (x === null || y === null || width === null || height === null || width < 0 || height < 0) return null;
  return { x, y, width, height };
}

export function parseViewport(value: unknown): PageViewport | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  const width = num(v.width);
  const height = num(v.height);
  if (width === null || height === null || width <= 0 || height <= 0) return null;
  const viewport: PageViewport = {
    width,
    height,
    scroll_x: num(v.scroll_x) ?? 0,
    scroll_y: num(v.scroll_y) ?? 0,
  };
  const pageWidth = num(v.page_width);
  const pageHeight = num(v.page_height);
  if (pageWidth !== null) viewport.page_width = pageWidth;
  if (pageHeight !== null) viewport.page_height = pageHeight;
  return viewport;
}

export function parseElement(value: unknown): PageElement | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  const rect = parseRect(v.rect);
  const selector = str(v.selector, 512).trim();
  if (!rect || !selector) return null;
  return {
    selector,
    tag: str(v.tag, 32).toLowerCase().replace(/[^a-z0-9-]/g, "") || "?",
    text: str(v.text, 600),
    html: str(v.html, 2000),
    role: str(v.role, 200),
    label: str(v.label, 200),
    heading: str(v.heading, 200),
    rect,
  };
}

/** One row of the selection tree. Labels and text are untrusted page strings. */
export interface TreeNode {
  /** Bridge-local id; only valid until the next answer replaces the tree. */
  node: number;
  tag: string;
  label: string;
  text: string;
  children: number;
}

/** The local tree around a selection: ancestors (outermost first), the
 *  selection itself, and its first element children. */
export interface SelectionTree {
  ancestors: TreeNode[];
  self: TreeNode;
  children: TreeNode[];
  childCount: number;
}

export const TREE_LIMITS = { ancestors: 8, children: 12, label: 48, text: 48, maxNode: 100_000 };

function parseNode(value: unknown): TreeNode | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (!Number.isInteger(v.node) || (v.node as number) < 0 || (v.node as number) > TREE_LIMITS.maxNode) return null;
  const children = Number.isInteger(v.children) && (v.children as number) >= 0 ? Math.min(v.children as number, 100_000) : 0;
  return {
    node: v.node as number,
    tag: str(v.tag, 32).toLowerCase().replace(/[^a-z0-9-]/g, "") || "?",
    label: str(v.label, TREE_LIMITS.label),
    text: str(v.text, TREE_LIMITS.text),
    children,
  };
}

export function parseTree(value: unknown): SelectionTree | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  const self = parseNode(v.self);
  if (!self) return null;
  const list = (items: unknown, max: number, fromEnd: boolean) => {
    if (!Array.isArray(items)) return [];
    const kept = fromEnd ? items.slice(-max) : items.slice(0, max);
    return kept.map(parseNode).filter((n): n is TreeNode => n !== null);
  };
  // Ids come from the page, which can make them repeat (or loop its
  // ancestry); each id appears once, the selection's own first.
  const seen = new Set([self.node]);
  const unique = (nodes: TreeNode[]) =>
    nodes.filter((n) => {
      if (seen.has(n.node)) return false;
      seen.add(n.node);
      return true;
    });
  // The nearest ancestors matter most: dedupe from the nearest outwards, then
  // keep at most the limit, outermost first.
  const ancestors = unique(list(v.ancestors, 64, true).reverse())
    .slice(0, TREE_LIMITS.ancestors)
    .reverse();
  const children = unique(list(v.children, 64, false)).slice(0, TREE_LIMITS.children);
  const count = Number.isInteger(v.child_count) ? Math.min(Math.max(v.child_count as number, 0), 100_000) : children.length;
  return {
    ancestors,
    self,
    children,
    childCount: Math.max(count, children.length),
  };
}

export type HelperEvent =
  | { type: "ready"; viewport: PageViewport | null }
  | {
      type: "picked";
      id: number;
      element: PageElement | null;
      /** Present with `element` when the bridge can select it again by id. */
      node: number | null;
      tree: SelectionTree | null;
      viewport: PageViewport | null;
    }
  /** `gone`: removed from the page. `hidden`: still there without a box of
   *  its own (display: none or contents, zero size). */
  | { type: "selection-status"; node: number; state: "ok" | "hidden" | "gone" }
  | {
      type: "probed";
      id: number;
      anchor: PageElement | null;
      elements: PageElement[];
      viewport: PageViewport | null;
    }
  | { type: "marks-status"; unresolved: number[] };

/** `null` for anything that is not a well-formed message from this load's bridge. */
export function parseHelperEvent(data: unknown, nonce: string): HelperEvent | null {
  if (!data || typeof data !== "object" || Array.isArray(data)) return null;
  const d = data as Record<string, unknown>;
  if (d[HELPER_NS] !== HELPER_VERSION || d.nonce !== nonce || typeof d.type !== "string") return null;
  const viewport = parseViewport(d.viewport);
  switch (d.type) {
    case "ready":
      return { type: "ready", viewport };
    case "picked": {
      if (typeof d.id !== "number") return null;
      const element = parseElement(d.element);
      const tree = element ? parseTree(d.tree) : null;
      const node = tree && d.node === tree.self.node ? tree.self.node : null;
      return { type: "picked", id: d.id, element, node, tree: node === null ? null : tree, viewport };
    }
    case "selection-status": {
      if (!Number.isInteger(d.node) || (d.state !== "ok" && d.state !== "hidden" && d.state !== "gone")) return null;
      return { type: "selection-status", node: d.node as number, state: d.state };
    }
    case "probed": {
      if (typeof d.id !== "number") return null;
      const elements = Array.isArray(d.elements)
        ? d.elements
            .slice(0, LIMITS.drawingElements)
            .map(parseElement)
            .filter((e): e is PageElement => e !== null)
        : [];
      return { type: "probed", id: d.id, anchor: parseElement(d.anchor), elements, viewport };
    }
    case "marks-status": {
      const unresolved = Array.isArray(d.unresolved)
        ? d.unresolved.filter((n): n is number => Number.isInteger(n) && n > 0 && n <= LIMITS.annotations)
        : [];
      return { type: "marks-status", unresolved };
    }
    default:
      return null;
  }
}

/** Mark positions for the bridge: geometry only, nothing the user wrote. */
export function marksFor(drafts: DraftAnnotation[]) {
  return drafts.flatMap((draft, index): { n: number; selector?: string; offset?: Rect; rect?: Rect }[] => {
    const n = index + 1;
    if (draft.target.kind === "element") return [{ n, selector: draft.target.element.selector }];
    if (draft.target.kind === "drawing") {
      const { anchor, offset, bounds } = draft.target;
      return anchor && offset ? [{ n, selector: anchor.selector, offset }] : [{ n, rect: bounds }];
    }
    return [];
  });
}

// --- drawing ---

export function strokeBounds(strokes: Point[][]): Rect | null {
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const stroke of strokes) {
    for (const [x, y] of stroke) {
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x);
      maxY = Math.max(maxY, y);
    }
  }
  if (!Number.isFinite(minX)) return null;
  return { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
}

/** Evenly spaced points along the strokes, plus the bounds' centre. */
export function samplePoints(strokes: Point[][], max = 64): Point[] {
  const all = strokes.flat();
  if (all.length === 0) return [];
  const step = Math.max(1, Math.floor(all.length / max));
  const out: Point[] = [];
  for (let i = 0; i < all.length; i += step) out.push(all[i]);
  const bounds = strokeBounds(strokes);
  if (bounds) out.push([bounds.x + bounds.width / 2, bounds.y + bounds.height / 2]);
  return out;
}

/** Stroke bounds (viewport px) relative to the anchor's box, so the mark follows it. */
export function anchorOffset(bounds: Rect, anchor: PageElement, viewport: PageViewport): Rect {
  return {
    x: bounds.x - (anchor.rect.x - viewport.scroll_x),
    y: bounds.y - (anchor.rect.y - viewport.scroll_y),
    width: bounds.width,
    height: bounds.height,
  };
}

/**
 * The picture the agent gets for a drawing: the viewport as a white page,
 * numbered outlines of the elements under the stroke (same order as the prompt
 * lists them), and the stroke in red. A schematic, not a screenshot: the
 * opaque-origin frame cannot be rasterized from the parent.
 */
export async function renderSketch(strokes: Point[][], elements: PageElement[], viewport: PageViewport): Promise<Blob> {
  const scale = Math.min(2, 1600 / Math.max(viewport.width, viewport.height, 1));
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(viewport.width * scale));
  canvas.height = Math.max(1, Math.round(viewport.height * scale));
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("Canvas is not available");
  ctx.scale(scale, scale);
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, viewport.width, viewport.height);
  ctx.font = "600 12px system-ui, sans-serif";
  elements.forEach((element, index) => {
    const x = element.rect.x - viewport.scroll_x;
    const y = element.rect.y - viewport.scroll_y;
    ctx.strokeStyle = "#8a8f89";
    ctx.lineWidth = 1.5;
    ctx.setLineDash([5, 4]);
    ctx.strokeRect(x, y, element.rect.width, element.rect.height);
    ctx.setLineDash([]);
    ctx.fillStyle = "#4b504a";
    ctx.fillText(`${index + 1} <${element.tag}>`, Math.max(2, x + 3), Math.max(12, y + 14));
  });
  ctx.strokeStyle = "#e5322d";
  ctx.lineWidth = 4;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  for (const stroke of strokes) {
    if (stroke.length === 0) continue;
    ctx.beginPath();
    ctx.moveTo(stroke[0][0], stroke[0][1]);
    for (const [x, y] of stroke.slice(1)) ctx.lineTo(x, y);
    if (stroke.length === 1) ctx.lineTo(stroke[0][0] + 0.1, stroke[0][1]);
    ctx.stroke();
  }
  return await new Promise<Blob>((resolve, reject) => {
    canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("Could not render the drawing"))), "image/png");
  });
}

// --- media ---

export async function sha256Hex(blob: Blob): Promise<string> {
  if (!crypto?.subtle) throw new Error("This browser cannot fingerprint the artifact (needs HTTPS).");
  const digest = await crypto.subtle.digest("SHA-256", await blob.arrayBuffer());
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
}

export async function blobToBase64(blob: Blob): Promise<string> {
  const bytes = new Uint8Array(await blob.arrayBuffer());
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

/**
 * Small PNG/JPEG/WebP pass as-is; anything else (HEIC, GIF, large photos) is
 * re-encoded as JPEG at most 2048 px, which the server's image path decodes.
 */
export async function prepareImage(file: Blob): Promise<Blob> {
  const maxEdge = 2048;
  const bitmap = await createImageBitmap(file);
  try {
    const fits = Math.max(bitmap.width, bitmap.height) <= maxEdge;
    if (fits && file.size <= 2 * 1024 * 1024 && /^image\/(png|jpeg|webp)$/.test(file.type)) return file;
    const scale = Math.min(1, maxEdge / Math.max(bitmap.width, bitmap.height));
    const canvas = document.createElement("canvas");
    canvas.width = Math.max(1, Math.round(bitmap.width * scale));
    canvas.height = Math.max(1, Math.round(bitmap.height * scale));
    canvas.getContext("2d")?.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    return await new Promise<Blob>((resolve, reject) => {
      canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("Could not read the image"))), "image/jpeg", 0.85);
    });
  } finally {
    bitmap.close();
  }
}

export function pickRecorderMime(): string {
  if (typeof MediaRecorder === "undefined") return "";
  for (const mime of ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/ogg;codecs=opus"]) {
    if (MediaRecorder.isTypeSupported?.(mime)) return mime;
  }
  return "";
}

export function canRecordVoice(): boolean {
  return typeof MediaRecorder !== "undefined" && Boolean(navigator.mediaDevices?.getUserMedia);
}

// --- payload ---

export interface FeedbackMediaPayload {
  kind: "audio" | "image";
  mime: string;
  data: string;
}

export interface FeedbackSubmissionPayload {
  submission_id: string;
  artifact_path: string;
  artifact_sha256: string;
  allow_stale: boolean;
  note: string;
  /** HTML artifacts. */
  viewport?: PageViewport;
  /** Image artifacts: the canonical copy the marks are fractions of. */
  image?: ImageFrame;
  annotations: {
    intent: FeedbackIntent;
    comment: string;
    target:
      | { kind: "element"; element: PageElement }
      | {
          kind: "drawing";
          bounds: Rect;
          anchor: PageElement | null;
          elements: PageElement[];
          sketch: FeedbackMediaPayload;
        }
      | ImagePinTarget
      | ImageAreaTarget
      | { kind: "page" };
    media: FeedbackMediaPayload[];
  }[];
}

async function media(kind: "audio" | "image", blob: Blob): Promise<FeedbackMediaPayload> {
  return { kind, mime: blob.type || (kind === "audio" ? "audio/webm" : "image/png"), data: await blobToBase64(blob) };
}

export async function buildPayload(input: {
  submissionId: string;
  artifactPath: string;
  sha256: string;
  allowStale: boolean;
  viewport: PageViewport | null;
  image?: ImageFrame | null;
  drafts: DraftAnnotation[];
}): Promise<FeedbackSubmissionPayload> {
  const annotations: FeedbackSubmissionPayload["annotations"] = [];
  for (const draft of input.drafts) {
    const target: FeedbackSubmissionPayload["annotations"][number]["target"] =
      draft.target.kind === "drawing"
        ? {
            kind: "drawing",
            bounds: draft.target.bounds,
            anchor: draft.target.anchor,
            elements: draft.target.elements,
            sketch: await media("image", draft.target.sketch),
          }
        : draft.target;
    const items: FeedbackMediaPayload[] = [];
    for (const clip of draft.voice) items.push(await media("audio", clip.blob));
    for (const image of draft.images) items.push(await media("image", image.blob));
    annotations.push({ intent: draft.intent, comment: draft.comment.trim(), target, media: items });
  }
  const payload: FeedbackSubmissionPayload = {
    submission_id: input.submissionId,
    artifact_path: input.artifactPath,
    artifact_sha256: input.sha256,
    allow_stale: input.allowStale,
    note: "",
    annotations,
  };
  if (input.image) {
    payload.image = { canonical_sha256: input.image.canonical_sha256, width: input.image.width, height: input.image.height };
  }
  else if (input.viewport) payload.viewport = input.viewport;
  return payload;
}

export function newId(): string {
  return crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 12)}`;
}

export function targetSummary(target: DraftTarget, pageWord = "page"): string {
  if (target.kind === "page") return `Whole ${pageWord}`;
  if (target.kind === "image_pin") {
    return `Pin at ${Math.round(target.point.x * 100)}%, ${Math.round(target.point.y * 100)}%`;
  }
  if (target.kind === "image_area") {
    const n = target.strokes.length;
    return `Marked area · ${n === 1 ? "1 stroke" : `${n} strokes`}`;
  }
  if (target.kind === "element") {
    const text = target.element.text.trim();
    return `<${target.element.tag}>${text ? ` ${text.slice(0, 60)}` : ""}`;
  }
  const n = target.elements.length;
  const where = target.anchor ? ` in <${target.anchor.tag}>` : "";
  return `Drawing${where} · ${n === 0 ? "no elements found" : n === 1 ? "1 element" : `${n} elements`}`;
}

/** Whether a draft says anything: a Keep mark may stand alone. */
export function draftIsEmpty(draft: Pick<DraftAnnotation, "intent" | "comment" | "voice" | "images">): boolean {
  return draft.intent !== "keep" && !draft.comment.trim() && draft.voice.length === 0 && draft.images.length === 0;
}
