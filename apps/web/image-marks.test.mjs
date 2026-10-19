import { expect, test } from "bun:test";
import { buildPayload, targetSummary } from "./src/lib/annotate.ts";
import {
  FIT_VIEW,
  IMAGE_LIMITS,
  badgePosition,
  revealBounds,
  finishStroke,
  fitRect,
  limitArea,
  panBy,
  toImage,
  toStage,
  viewRect,
  zoomAround,
} from "./src/lib/imageMarks.ts";

const close = (a, b) => expect(Math.abs(a - b)).toBeLessThan(1e-6);

test("a landscape image on a portrait stage is letterboxed, and the margin is outside the image", () => {
  const stage = { width: 390, height: 600 };
  const natural = { width: 1200, height: 800 };
  const rect = fitRect(stage, natural);
  expect(rect).toEqual({ x: 0, y: 170, width: 390, height: 260 });
  expect(toImage(rect, 195, 300)).toEqual({ x: 0.5, y: 0.5 });
  // A tap in the black band above the image is not on the image.
  expect(toImage(rect, 100, 100).y).toBeLessThan(0);
  // A portrait image on a landscape stage is pillarboxed.
  expect(fitRect({ width: 800, height: 400 }, { width: 300, height: 600 })).toEqual({ x: 300, y: 0, width: 200, height: 400 });
});

test("zoom keeps the point under the fingers, and pan stops at the image edges", () => {
  const stage = { width: 390, height: 600 };
  const natural = { width: 1200, height: 800 };
  const point = { x: 0.8, y: 0.5 };
  const [fx, fy] = toStage(fitRect(stage, natural), point);
  const zoomed = zoomAround(stage, natural, 3, point, fx, fy);
  expect(zoomed.zoom).toBe(3);
  const after = toStage(viewRect(stage, natural, zoomed).rect, point);
  close(after[0], fx);
  close(after[1], fy);

  // No zoom below fit; at fit the image stays centred whatever the centre says.
  expect(viewRect(stage, natural, { zoom: 0.2, cx: 0.9, cy: 0.1 }).rect).toEqual(fitRect(stage, natural));
  // Dragging far right at 3× stops with the image's left edge at the stage edge.
  const panned = viewRect(stage, natural, panBy(stage, natural, zoomed, 5000, 0)).rect;
  expect(panned.x).toBe(0);
  const pannedLeft = viewRect(stage, natural, panBy(stage, natural, zoomed, -5000, 0)).rect;
  close(pannedLeft.x + pannedLeft.width, stage.width);
});

test("a mark stays on the same image spot across resize and rotation", () => {
  const natural = { width: 1200, height: 800 };
  const mark = { x: 0.62, y: 0.41 };
  const view = zoomAround({ width: 390, height: 600 }, natural, 2.5, mark, 200, 300);
  for (const stage of [
    { width: 390, height: 600 },
    { width: 844, height: 300 },
    { width: 1024, height: 700 },
  ]) {
    const { rect, view: shown } = viewRect(stage, natural, view);
    const [x, y] = toStage(rect, mark);
    const back = toImage(rect, x, y);
    close(back.x, mark.x);
    close(back.y, mark.y);
    expect(shown.zoom).toBe(2.5);
  }
  expect(viewRect({ width: 390, height: 600 }, natural, FIT_VIEW).view.zoom).toBe(1);
});

test("strokes are clamped to the image edge, dropped when only in the margin, and thinned", () => {
  expect(finishStroke([{ x: -0.2, y: 0.5 }, { x: -0.1, y: 0.6 }])).toBeNull();
  expect(finishStroke([{ x: -0.2, y: 0.5 }, { x: 0.3, y: 0.9 }, { x: 0.3, y: 1.4 }])).toEqual([
    { x: 0, y: 0.5 },
    { x: 0.3, y: 0.9 },
    { x: 0.3, y: 1 },
  ]);
  const long = Array.from({ length: 2000 }, (_, i) => ({ x: i / 2000, y: 0.5 }));
  const thinned = finishStroke(long);
  expect(thinned.length).toBe(IMAGE_LIMITS.strokePoints);
  expect(thinned[0]).toEqual(long[0]);
  expect(thinned.at(-1)).toEqual({ x: 0.9995, y: 0.5 });
  const area = limitArea(Array.from({ length: 10 }, () => thinned));
  expect(area.reduce((sum, stroke) => sum + stroke.length, 0)).toBeLessThanOrEqual(IMAGE_LIMITS.areaPoints);
});

test("an image payload carries the displayed size and image targets, not a viewport", async () => {
  const payload = await buildPayload({
    submissionId: "sub-12345678",
    artifactPath: "chart.png",
    sha256: "a".repeat(64),
    allowStale: false,
    viewport: { width: 390, height: 600, scroll_x: 0, scroll_y: 0 },
    image: { canonical_sha256: "b".repeat(64), width: 1200, height: 800 },
    drafts: [
      { id: "1", intent: "change", comment: " bolder ", target: { kind: "image_pin", point: { x: 0.25, y: 0.5 } }, voice: [], images: [] },
      { id: "2", intent: "keep", comment: "", target: { kind: "image_area", strokes: [[{ x: 0.1, y: 0.1 }]] }, voice: [], images: [] },
      { id: "3", intent: "note", comment: "ok", target: { kind: "page" }, voice: [], images: [] },
    ],
  });
  expect(payload.image).toEqual({ canonical_sha256: "b".repeat(64), width: 1200, height: 800 });
  expect(payload.viewport).toBeUndefined();
  expect(payload.annotations.map((a) => a.target.kind)).toEqual(["image_pin", "image_area", "page"]);
  expect(payload.annotations[0].comment).toBe("bolder");
  expect(targetSummary({ kind: "image_pin", point: { x: 0.25, y: 0.5 } })).toBe("Pin at 25%, 50%");
  expect(targetSummary({ kind: "page" }, "image")).toBe("Whole image");
});

test("a pin near the bottom stays visible when the editor shrinks the stage while zoomed", () => {
  const natural = { width: 1200, height: 800 };
  const tall = { width: 390, height: 545 };
  const short = { width: 390, height: 297 };
  // Zoomed 2.5×, a pin tapped 30 px above the bottom of the tall stage.
  const view = zoomAround(tall, natural, 2.5, { x: 0.5, y: 0.5 }, 195, 272);
  const pin = toImage(viewRect(tall, natural, view).rect, 200, tall.height - 30);
  // Without revealing, the shorter stage puts it below the visible area.
  const [, hiddenY] = toStage(viewRect(short, natural, view).rect, pin);
  expect(hiddenY).toBeGreaterThan(short.height);
  const revealed = revealBounds(short, natural, view, { x0: pin.x, y0: pin.y, x1: pin.x, y1: pin.y }, 36);
  const { rect } = viewRect(short, natural, revealed);
  const [x, y] = toStage(rect, pin);
  expect(y).toBeGreaterThanOrEqual(36);
  expect(y).toBeLessThanOrEqual(short.height - 36 + 0.5);
  expect(x).toBeCloseTo(200, 5);
  expect(revealed.zoom).toBe(2.5);
  // Already in view: the reader's view is left alone.
  expect(revealBounds(short, natural, revealed, { x0: pin.x, y0: pin.y, x1: pin.x, y1: pin.y }, 36)).toEqual(
    viewRect(short, natural, revealed).view,
  );
});

test("a mark's number stays beside the mark, inside the image, never pinned to the stage edge", () => {
  const rect = { x: -300, y: -200, width: 1170, height: 780 };
  // A pin far below the visible stage: its number is far below too.
  const [, by] = badgePosition(rect, { kind: "image_pin", point: { x: 0.5, y: 0.95 } });
  expect(by).toBeGreaterThan(500);
  // At the image's right edge the number moves inside the image.
  const [bx] = badgePosition(rect, { kind: "image_pin", point: { x: 1, y: 0.5 } });
  expect(bx).toBe(rect.x + rect.width - 11);
});
