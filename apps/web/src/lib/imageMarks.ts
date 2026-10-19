/**
 * Image artifact annotation: view geometry (fit, zoom, pan) and the mapping
 * between stage pixels and image-relative points.
 *
 * The image shown is the server's canonical copy of the artifact (`GET
 * /api/threads/{id}/artifacts/image`): small, upright, without EXIF. The app
 * shows it as is and never re-encodes or re-orients it, so marks, stored as
 * fractions of that copy (`0,0` top-left, `1,1` bottom-right), name exactly
 * the pixels the agent gets. Formats and limits are the server's call. Marks
 * never store screen pixels, so they stay attached through zoom, pan,
 * resize, and rotation: only the view changes.
 *
 * Kept free of `$lib` imports so `bun test` can load it directly.
 */

/** A spot on the image as fractions of its displayed width and height. */
export interface ImagePoint {
  x: number;
  y: number;
}

/** A note's mark on the stage, or the one being edited (`n: null`, highlighted). */
export interface StageMark {
  n: number | null;
  target: { kind: "image_pin"; point: ImagePoint } | { kind: "image_area"; strokes: ImagePoint[][] };
}

export interface Size {
  width: number;
  height: number;
}

/** A rectangle in stage (CSS) pixels. */
export interface StageRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * What the user looks at: `zoom` over the fitted size, and the image point
 * shown at the centre of the stage. Image-relative, so it survives resizes.
 */
export interface ImageView {
  zoom: number;
  cx: number;
  cy: number;
}

export const FIT_VIEW: ImageView = { zoom: 1, cx: 0.5, cy: 0.5 };

/** Mirrors `zdx_engine::core::artifact_feedback` limits for image artifacts. */
export const IMAGE_LIMITS = {
  strokes: 24,
  strokePoints: 600,
  areaPoints: 4000,
};

/** The image fitted inside the stage (`contain`), centred: letterboxed or pillarboxed. */
export function fitRect(stage: Size, natural: Size): StageRect {
  const scale = Math.min(stage.width / Math.max(1, natural.width), stage.height / Math.max(1, natural.height));
  const width = natural.width * scale;
  const height = natural.height * scale;
  return { x: (stage.width - width) / 2, y: (stage.height - height) / 2, width, height };
}

/** Enough zoom to see single pixels clearly, at least 4×, at most 40×. */
export function maxZoom(stage: Size, natural: Size): number {
  const fitted = fitRect(stage, natural);
  const toPixels = natural.width / Math.max(1, fitted.width);
  return Math.min(40, Math.max(4, toPixels * 4));
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/**
 * Where the image is on the stage for a view, plus the view itself after
 * clamping: no zoom below fit, and no panning past the image edges (an image
 * smaller than the stage on an axis stays centred on it).
 */
export function viewRect(stage: Size, natural: Size, view: ImageView): { rect: StageRect; view: ImageView } {
  const base = fitRect(stage, natural);
  const zoom = clamp(Number.isFinite(view.zoom) ? view.zoom : 1, 1, maxZoom(stage, natural));
  const width = base.width * zoom;
  const height = base.height * zoom;
  const place = (size: number, room: number, centre: number) =>
    size <= room ? (room - size) / 2 : clamp(room / 2 - centre * size, room - size, 0);
  const x = place(width, stage.width, Number.isFinite(view.cx) ? view.cx : 0.5);
  const y = place(height, stage.height, Number.isFinite(view.cy) ? view.cy : 0.5);
  const rect = { x, y, width, height };
  return {
    rect,
    view: { zoom, cx: (stage.width / 2 - x) / width, cy: (stage.height / 2 - y) / height },
  };
}

/** Stage point to image point. May fall outside `0..1` (letterbox area). */
export function toImage(rect: StageRect, stageX: number, stageY: number): ImagePoint {
  return { x: (stageX - rect.x) / rect.width, y: (stageY - rect.y) / rect.height };
}

export function toStage(rect: StageRect, point: ImagePoint): [number, number] {
  return [rect.x + point.x * rect.width, rect.y + point.y * rect.height];
}

export function insideImage(point: ImagePoint): boolean {
  return point.x >= 0 && point.x <= 1 && point.y >= 0 && point.y <= 1;
}

/** Clamped to the image edge and rounded to 1/100000 (sub-pixel at 40 MP). */
export function clampPoint(point: ImagePoint): ImagePoint {
  const round = (value: number) => Math.round(clamp(value, 0, 1) * 100_000) / 100_000;
  return { x: round(point.x), y: round(point.y) };
}

/**
 * Zooms to `zoom` keeping `image` (an image point) under `stageX, stageY`:
 * pinch and wheel zoom around the fingers or the cursor.
 */
export function zoomAround(
  stage: Size,
  natural: Size,
  zoom: number,
  image: ImagePoint,
  stageX: number,
  stageY: number,
): ImageView {
  const base = fitRect(stage, natural);
  const z = clamp(zoom, 1, maxZoom(stage, natural));
  const width = base.width * z;
  const height = base.height * z;
  const x = stageX - image.x * width;
  const y = stageY - image.y * height;
  return viewRect(stage, natural, { zoom: z, cx: (stage.width / 2 - x) / width, cy: (stage.height / 2 - y) / height }).view;
}

/** Moves the view by a finger drag of `dx, dy` stage pixels. */
export function panBy(stage: Size, natural: Size, view: ImageView, dx: number, dy: number): ImageView {
  const { rect } = viewRect(stage, natural, view);
  const x = rect.x + dx;
  const y = rect.y + dy;
  return viewRect(stage, natural, {
    zoom: view.zoom,
    cx: (stage.width / 2 - x) / rect.width,
    cy: (stage.height / 2 - y) / rect.height,
  }).view;
}

/** Bounds of a mark in image fractions. */
export function markBounds(target: StageMark["target"]): { x0: number; y0: number; x1: number; y1: number } | null {
  if (target.kind === "image_pin") return { x0: target.point.x, y0: target.point.y, x1: target.point.x, y1: target.point.y };
  return strokesBounds(target.strokes);
}

/**
 * The smallest pan that brings `bounds` (image fractions), plus `margin`
 * stage px around it, into the stage. Zoom never changes. When the mark is
 * larger than the stage, its centre is centred. The view is unchanged when
 * the mark is already in view, so a reader's own panning is left alone.
 */
export function revealBounds(
  stage: Size,
  natural: Size,
  view: ImageView,
  bounds: { x0: number; y0: number; x1: number; y1: number },
  margin: number,
): ImageView {
  const { rect, view: shown } = viewRect(stage, natural, view);
  const [ax, ay] = toStage(rect, { x: bounds.x0, y: bounds.y0 });
  const [bx, by] = toStage(rect, { x: bounds.x1, y: bounds.y1 });
  const shift = (lo: number, hi: number, room: number) => {
    const a = lo - margin;
    const b = hi + margin;
    if (b - a > room) return room / 2 - (lo + hi) / 2;
    if (a < 0) return -a;
    if (b > room) return room - b;
    return 0;
  };
  const dx = shift(ax, bx, stage.width);
  const dy = shift(ay, by, stage.height);
  if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5) return shown;
  return panBy(stage, natural, shown, dx, dy);
}

/**
 * Where a mark's number goes, in stage px: up and right of a pin, up and left
 * of an area, kept inside the image's on-stage box so it always sits next to
 * its own mark. When the mark is off screen, so is its number.
 */
export function badgePosition(rect: StageRect, target: StageMark["target"], radius = 11): [number, number] {
  let x: number;
  let y: number;
  if (target.kind === "image_pin") {
    [x, y] = toStage(rect, target.point);
    x += 16;
    y -= 16;
  } else {
    const bounds = strokesBounds(target.strokes);
    [x, y] = bounds ? toStage(rect, { x: bounds.x0, y: bounds.y0 }) : [rect.x, rect.y];
    x -= 10;
    y -= 10;
  }
  const clampTo = (value: number, lo: number, hi: number) => (hi < lo ? (lo + hi) / 2 : Math.min(hi, Math.max(lo, value)));
  return [clampTo(x, rect.x + radius, rect.x + rect.width - radius), clampTo(y, rect.y + radius, rect.y + rect.height - radius)];
}

/**
 * A finished stroke as the engine takes it: `null` when none of it touched
 * the image (drawn in the letterbox), otherwise clamped to the image edge and
 * thinned to the point limit.
 */
export function finishStroke(points: ImagePoint[]): ImagePoint[] | null {
  if (!points.some(insideImage)) return null;
  return thin(points.map(clampPoint), IMAGE_LIMITS.strokePoints);
}

/** Evenly spaced points, first and last kept, at most `max`. */
function thin(points: ImagePoint[], max: number): ImagePoint[] {
  if (points.length <= max) return points;
  if (max <= 1) return points.slice(0, 1);
  const step = (points.length - 1) / (max - 1);
  return Array.from({ length: max }, (_, i) => points[Math.round(i * step)]);
}

/** Thins every stroke by the same share so the area fits the engine's point limit. */
export function limitArea(strokes: ImagePoint[][]): ImagePoint[][] {
  const total = strokes.reduce((sum, stroke) => sum + stroke.length, 0);
  if (total <= IMAGE_LIMITS.areaPoints) return strokes;
  const share = IMAGE_LIMITS.areaPoints / total;
  return strokes.map((stroke) => thin(stroke, Math.max(1, Math.floor(stroke.length * share))));
}

/** Bounds of strokes in image fractions. */
export function strokesBounds(strokes: ImagePoint[][]): { x0: number; y0: number; x1: number; y1: number } | null {
  const all = strokes.flat();
  if (all.length === 0) return null;
  return {
    x0: Math.min(...all.map((p) => p.x)),
    y0: Math.min(...all.map((p) => p.y)),
    x1: Math.max(...all.map((p) => p.x)),
    y1: Math.max(...all.map((p) => p.y)),
  };
}

export function percent(value: number): string {
  return `${Math.round(value * 100)}%`;
}
