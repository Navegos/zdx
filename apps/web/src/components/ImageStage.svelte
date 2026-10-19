<script lang="ts">
  import { onMount, untrack } from "svelte";
  import {
    FIT_VIEW,
    badgePosition,
    markBounds,
    revealBounds,
    clampPoint,
    finishStroke,
    insideImage,
    panBy,
    toImage,
    toStage,
    viewRect,
    zoomAround,
    IMAGE_LIMITS,
    type ImagePoint,
    type ImageView,
    type Size,
    type StageMark,
  } from "$lib/imageMarks";

  interface Props {
    src: string;
    alt: string;
    /** Displayed (EXIF-upright) image size. */
    natural: Size;
    /** `select` places a pin on tap; `draw` draws strokes. Zoom and pan work in every mode. */
    mode: "browse" | "select" | "draw";
    /** Whether a tap may place or move a pin right now. */
    tapEnabled: boolean;
    /** Whether a finger may draw right now. */
    drawEnabled: boolean;
    marks: StageMark[];
    /** The mark being edited. Kept in view when it changes and when the stage
     *  resizes (the editor opening, rotation). */
    focus?: StageMark["target"] | null;
    /** Strokes drawn but not yet turned into a note, in image fractions. */
    strokes?: ImagePoint[][];
    onpin: (point: ImagePoint) => void;
    onhint: (text: string) => void;
  }

  let {
    src,
    alt,
    natural,
    mode,
    tapEnabled,
    drawEnabled,
    marks,
    focus = null,
    strokes = $bindable([]),
    onpin,
    onhint,
  }: Props = $props();

  let el = $state<HTMLDivElement | null>(null);
  let width = $state(0);
  let height = $state(0);
  let view = $state<ImageView>({ ...FIT_VIEW });
  /** The stroke under the finger right now. */
  let live = $state<ImagePoint[] | null>(null);

  const stage = $derived<Size>({ width, height });
  const placed = $derived(width > 0 && height > 0 ? viewRect(stage, natural, view) : null);
  const rect = $derived(placed?.rect ?? null);
  const zoomed = $derived((placed?.view.zoom ?? 1) > 1.01);

  type Gesture =
    | { kind: "tap"; id: number; x: number; y: number; lastX: number; lastY: number; moved: boolean }
    | { kind: "pinch"; d0: number; z0: number; image: ImagePoint }
    | { kind: "draw"; id: number; lastX: number; lastY: number };
  const pointers = new Map<number, { x: number; y: number }>();
  let gesture: Gesture | null = null;
  let lastTap: { at: number; x: number; y: number } | null = null;

  // A new image starts fitted.
  $effect(() => {
    void src;
    view = { ...FIT_VIEW };
  });

  /** Fit sits in a top corner. It moves to the other one when a mark or its
   *  number is under it, so it never hides the mark being worked on; when
   *  both corners are taken it picks the one hiding fewer marks. */
  const FIT_ZONE = { width: 64, height: 48 };
  const fitCorner = $derived.by((): "right" | "left" => {
    if (!rect || !zoomed) return "right";
    const spots: [number, number][] = [];
    for (const mark of marks) {
      spots.push(badgePosition(rect, mark.target));
      if (mark.target.kind === "image_pin") spots.push(toStage(rect, mark.target.point));
      else for (const stroke of mark.target.strokes) for (const point of stroke) spots.push(toStage(rect, point));
    }
    const near = 14;
    const under = (left: number) =>
      spots.filter(([x, y]) => x >= left - near && x <= left + FIT_ZONE.width + near && y <= FIT_ZONE.height + near).length;
    const right = under(width - FIT_ZONE.width);
    if (right === 0) return "right";
    return under(0) < right ? "left" : "right";
  });

  /** Room kept around a revealed mark: the pin ring and its number. */
  const REVEAL_MARGIN = 36;

  // The editor opening shrinks the stage, and rotation reshapes it: either
  // can push the mark being edited off screen, most of all when zoomed in.
  // Pan just enough to bring it back. Unfinished strokes count when nothing
  // is being edited. The current view is read untracked, so the reader's own
  // panning never triggers this.
  $effect(() => {
    const target = focus;
    if (width === 0 || height === 0) return;
    untrack(() => {
      const mark = target ?? (strokes.length > 0 ? { kind: "image_area" as const, strokes } : null);
      const bounds = mark ? markBounds(mark) : null;
      if (bounds) view = revealBounds(stage, natural, view, bounds, REVEAL_MARGIN);
    });
  });

  function local(event: PointerEvent | WheelEvent): { x: number; y: number } {
    const box = el!.getBoundingClientRect();
    return { x: event.clientX - box.left, y: event.clientY - box.top };
  }

  function setView(next: ImageView) {
    // Store the clamped view so a later pan starts from what is on screen.
    view = viewRect(stage, natural, next).view;
  }

  function pinchStart() {
    const [a, b] = [...pointers.values()];
    if (!rect || !a || !b) return;
    live = null;
    const mid = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
    gesture = {
      kind: "pinch",
      d0: Math.max(1, Math.hypot(a.x - b.x, a.y - b.y)),
      z0: placed!.view.zoom,
      image: toImage(rect, mid.x, mid.y),
    };
  }

  function onDown(event: PointerEvent) {
    if (!el || !rect) return;
    if (event.pointerType === "mouse" && event.button !== 0) return;
    if ((event.target as HTMLElement).closest(".fit-btn")) return;
    event.preventDefault();
    try {
      el.setPointerCapture(event.pointerId);
    } catch {
      // A pointer that is already gone cannot be captured; its events still arrive.
    }
    const p = local(event);
    pointers.set(event.pointerId, p);
    if (pointers.size === 2) {
      // A second finger always means zoom: a stroke just started is dropped.
      pinchStart();
      return;
    }
    if (pointers.size > 2) return;
    if (mode === "draw" && drawEnabled) {
      if (strokes.length >= IMAGE_LIMITS.strokes) {
        onhint(`Up to ${IMAGE_LIMITS.strokes} strokes per note. Tap Comment.`);
        gesture = null;
        return;
      }
      gesture = { kind: "draw", id: event.pointerId, lastX: p.x, lastY: p.y };
      live = [toImage(rect, p.x, p.y)];
    } else {
      gesture = { kind: "tap", id: event.pointerId, x: p.x, y: p.y, lastX: p.x, lastY: p.y, moved: false };
    }
  }

  function onMove(event: PointerEvent) {
    if (!pointers.has(event.pointerId) || !rect) return;
    event.preventDefault();
    const p = local(event);
    pointers.set(event.pointerId, p);
    const g = gesture;
    if (!g) return;
    if (g.kind === "pinch") {
      const [a, b] = [...pointers.values()];
      if (!a || !b) return;
      const d = Math.hypot(a.x - b.x, a.y - b.y);
      setView(zoomAround(stage, natural, g.z0 * (d / g.d0), g.image, (a.x + b.x) / 2, (a.y + b.y) / 2));
    } else if (g.kind === "draw" && g.id === event.pointerId && live) {
      if (Math.hypot(p.x - g.lastX, p.y - g.lastY) < 1.5) return;
      g.lastX = p.x;
      g.lastY = p.y;
      if (live.length < IMAGE_LIMITS.strokePoints * 4) live.push(toImage(rect, p.x, p.y));
    } else if (g.kind === "tap" && g.id === event.pointerId) {
      if (!g.moved && Math.hypot(p.x - g.x, p.y - g.y) > 8) g.moved = true;
      if (g.moved) {
        setView(panBy(stage, natural, placed!.view, p.x - g.lastX, p.y - g.lastY));
        g.lastX = p.x;
        g.lastY = p.y;
      }
    }
  }

  function onUp(event: PointerEvent) {
    if (!pointers.has(event.pointerId)) return;
    pointers.delete(event.pointerId);
    const g = gesture;
    const cancelled = event.type === "pointercancel";
    if (g?.kind === "pinch") {
      // The finger left on the glass pans; it never taps or draws.
      const [rest] = [...pointers.entries()];
      gesture = rest ? { kind: "tap", id: rest[0], x: rest[1].x, y: rest[1].y, lastX: rest[1].x, lastY: rest[1].y, moved: true } : null;
      return;
    }
    if (!g || ("id" in g && g.id !== event.pointerId)) return;
    gesture = null;
    if (g.kind === "draw") {
      const points = live;
      live = null;
      if (cancelled || !points) return;
      const stroke = finishStroke(points);
      if (stroke) strokes = [...strokes, stroke];
      else onhint("Draw on the image, not the margin around it.");
      return;
    }
    if (g.moved || cancelled || !rect) return;
    const p = local(event);
    if (mode === "select") {
      if (!tapEnabled) return;
      const point = toImage(rect, p.x, p.y);
      if (insideImage(point)) onpin(clampPoint(point));
      else onhint("Tap on the image to place a pin.");
      return;
    }
    if (mode === "browse") {
      const now = performance.now();
      if (lastTap && now - lastTap.at < 320 && Math.hypot(p.x - lastTap.x, p.y - lastTap.y) < 30) {
        lastTap = null;
        if (zoomed) setView({ ...FIT_VIEW });
        else setView(zoomAround(stage, natural, 2.5, toImage(rect, p.x, p.y), p.x, p.y));
      } else {
        lastTap = { at: now, x: p.x, y: p.y };
      }
    }
  }

  function onWheel(event: WheelEvent) {
    if (!rect) return;
    event.preventDefault();
    const p = local(event);
    const factor = Math.exp(-event.deltaY * (event.ctrlKey ? 0.01 : 0.0015));
    setView(zoomAround(stage, natural, placed!.view.zoom * factor, toImage(rect, p.x, p.y), p.x, p.y));
  }

  onMount(() => {
    // Not a Svelte `onwheel`: it must be non-passive to stop the page zooming.
    el?.addEventListener("wheel", onWheel, { passive: false });
    return () => el?.removeEventListener("wheel", onWheel);
  });

  function path(points: ImagePoint[]): string {
    if (!rect) return "";
    return points.map((point) => toStage(rect, point).map((v) => v.toFixed(1)).join(",")).join(" ");
  }

</script>

<div
  class="img-stage"
  class:drawing={mode === "draw"}
  class:picking={mode === "select"}
  bind:this={el}
  bind:clientWidth={width}
  bind:clientHeight={height}
  role="presentation"
  onpointerdown={onDown}
  onpointermove={onMove}
  onpointerup={onUp}
  onpointercancel={onUp}
>
  {#if rect}
    <img
      {src}
      {alt}
      draggable="false"
      class="img"
      style="left: {rect.x}px; top: {rect.y}px; width: {rect.width}px; height: {rect.height}px;"
    />
    <svg class="marks" {width} {height} aria-hidden="true">
      {#each marks as mark, i (i)}
        {@const color = mark.n === null ? "#286bbd" : "#e5322d"}
        {@const [bx, by] = badgePosition(rect, mark.target)}
        {#if mark.target.kind === "image_pin"}
          {@const [x, y] = toStage(rect, mark.target.point)}
          <circle cx={x} cy={y} r="11" fill="none" stroke="white" stroke-width="6" />
          <circle cx={x} cy={y} r="11" fill="none" stroke={color} stroke-width="3" />
        {:else}
          {#each mark.target.strokes as stroke, j (j)}
            <polyline points={path(stroke)} class="halo" />
          {/each}
          {#each mark.target.strokes as stroke, j (j)}
            <polyline points={path(stroke)} class="ink" style="stroke: {color}" />
          {/each}
        {/if}
        <circle cx={bx} cy={by} r="10" fill={color} stroke="white" stroke-width="2" />
        <text x={bx} y={by} class="badge-text">{mark.n ?? "•"}</text>
      {/each}
      {#each strokes as stroke, j (j)}
        <polyline points={path(stroke)} class="halo" />
        <polyline points={path(stroke)} class="ink" />
      {/each}
      {#if live && live.length > 0}
        <polyline points={path(live.length === 1 ? [live[0], live[0]] : live)} class="halo" />
        <polyline points={path(live.length === 1 ? [live[0], live[0]] : live)} class="ink" />
      {/if}
    </svg>
    {#if zoomed}
      <button type="button" class="fit-btn" class:left={fitCorner === "left"} onclick={() => setView({ ...FIT_VIEW })}>
        Fit
      </button>
    {/if}
  {/if}
</div>

<style>
  .img-stage {
    position: absolute;
    inset: 0;
    overflow: hidden;
    touch-action: none;
    user-select: none;
    -webkit-user-select: none;
    -webkit-touch-callout: none;
    background: var(--color-muted, #f1f1f1);
  }

  .img-stage.picking,
  .img-stage.drawing {
    cursor: crosshair;
  }

  .img {
    position: absolute;
    max-width: none;
    pointer-events: none;
  }

  .marks {
    position: absolute;
    inset: 0;
    pointer-events: none;
    overflow: visible;
  }

  .halo,
  .ink {
    fill: none;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .halo {
    stroke: white;
    stroke-width: 7;
  }

  .ink {
    stroke: #e5322d;
    stroke-width: 3.5;
  }

  .badge-text {
    fill: white;
    font: 600 11px system-ui, sans-serif;
    text-anchor: middle;
    dominant-baseline: central;
  }

  .fit-btn.left {
    right: auto;
    left: 0.5rem;
  }

  .fit-btn {
    position: absolute;
    top: 0.5rem;
    right: 0.5rem;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.66);
    padding: 0.3rem 0.6rem;
    font-size: 0.75rem;
    color: white;
  }
</style>
