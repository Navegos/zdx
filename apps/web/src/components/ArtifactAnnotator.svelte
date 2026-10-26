<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, ApiError } from "$lib/api";
  import {
    INTENTS,
    LIMITS,
    anchorOffset,
    buildPayload,
    canRecordVoice,
    draftIsEmpty,
    helperMessage,
    injectHelper,
    marksFor,
    newId,
    newNonce,
    parseHelperEvent,
    pickRecorderMime,
    quotePreview,
    prepareImage,
    renderSketch,
    samplePoints,
    sha256Hex,
    strokeBounds,
    targetDrawsPixels,
    targetSummary,
    type DraftAnnotation,
    type DraftTarget,
    type ImageFrame,
    type PageElement,
    type InspectedItem,
    type MeasuredRange,
    type PageViewport,
    type Rect,
    type Point,
    type SelectionTree,
    type TextSelection,
    type TreeNode,
  } from "$lib/annotate";
  import { draftKey, loadDraftRecord, saveDraftRecord } from "$lib/draftStore";
  import {
    allMatched,
    carryOver,
    checkHtmlNotes,
    acceptMeasured,
    checkImageNotes,
    inspectPlan,
    rangeRequests,
    type InspectPlan,
    type NoteCheck,
  } from "$lib/versionMatch";
  import { limitArea, type ImagePoint, type Size, type StageMark } from "$lib/imageMarks";
  import { confirmAction, haptic, notify, setClosingConfirmation, telegramUserId } from "$lib/telegram";
  import type { ArtifactAttachment, ArtifactImageCopy, ArtifactVersion } from "$lib/types";
  import Icon from "./Icon.svelte";
  import ImageStage from "./ImageStage.svelte";

  interface Props {
    threadId: string;
    artifact: ArtifactAttachment;
    /** HTML: the artifact bytes exactly as served, the version the user
     *  annotates. Image: the canonical copy's bytes (`image.blob`). */
    blob: Blob;
    /** Image artifacts: the server's canonical copy and its identity. The
     *  copy is shown as is; marks are fractions of it. */
    image?: ArtifactImageCopy;
    /** Work that closing would lose: an open note, a recording, or a stroke. */
    unsaved?: boolean;
  }

  let { threadId, artifact, blob, image, unsaved = $bindable(false) }: Props = $props();

  type Mode = "browse" | "select" | "draw";
  type Panel = "none" | "editor" | "list" | "update";
  /** `reopened`: the notes were made on an earlier version than the one shown.
   *  `server`: the file on the bot host changed after this version was opened. */
  type Stale = null | "reopened" | "server";

  let frame = $state<HTMLIFrameElement | null>(null);
  let stage = $state<HTMLDivElement | null>(null);
  let canvas = $state<HTMLCanvasElement | null>(null);
  let frameUrl = $state<string | null>(null);
  let pageHtml = "";
  let nonce = "";
  /** SHA-256 of the bytes on screen. */
  let sha = $state<string | null>(null);
  let prepareError = $state("");
  let loaded = $state(false);

  /** The image copy shown now: the prop at first, a newer copy after Update. */
  let imageCopy = $state<ArtifactImageCopy | undefined>(untrack(() => image));
  /** Image artifacts: pins and drawn areas on the canonical copy, no page bridge. */
  const isImage = $derived(imageCopy !== undefined);
  const surface = $derived(isImage ? "image" : "page");
  let imageUrl = $state<string | null>(null);
  /** The copy's size, as the server named it (checked against the decode). */
  let imageSize = $state<Size | null>(null);
  /** The copy shown now, as the payload names it. */
  const currentFrame = $derived<ImageFrame | null>(
    imageCopy
      ? { canonical_sha256: imageCopy.canonicalSha256, width: imageCopy.width, height: imageCopy.height }
      : null,
  );
  /** Strokes drawn on the image that are not a note yet. */
  let imageStrokes = $state<ImagePoint[][]>([]);

  let mode = $state<Mode>("browse");
  let panel = $state<Panel>("none");
  let helperReady = $state(false);
  let bridgeLost = $state(false);
  let lastViewport = $state<PageViewport | null>(null);
  let unresolved = $state<number[]>([]);
  let hint = $state("");

  let recordKey = "";
  let drafts = $state<DraftAnnotation[]>([]);
  /** Version the drafts were made on; differs from `sha` only when stale. */
  let draftSha = $state<string | null>(null);
  /** Image artifacts: the copy the drafts' marks are fractions of. Differs
   *  from `currentFrame` only when stale; never silently moved. */
  let draftFrame = $state<ImageFrame | null>(null);
  let submissionId = newId();
  let stale = $state<Stale>(null);
  let editing = $state<DraftAnnotation | null>(null);
  let editingIsNew = $state(false);
  let editorError = $state("");

  let sendState = $state<"idle" | "sending" | "sent" | "error">("idle");
  let sendMessage = $state("");
  /** The drafts are frozen while a send is in flight: the server gets exactly
   *  the snapshot whose `submission_id` it will remember. */
  const sending = $derived(sendState === "sending");
  /** Where the drafts on screen stand on the device: `saving` until the
   *  IndexedDB transaction commits (a reload before that loses the change),
   *  `failed` when the write did not commit (memory only). */
  let storage = $state<"saved" | "saving" | "failed">("saved");
  let persistSeq = 0;

  let recording = $state(false);
  let recordMs = $state(0);
  let recorder: MediaRecorder | null = null;
  let recordTimer: ReturnType<typeof setInterval> | null = null;
  let recordStream: MediaStream | null = null;
  const voiceSupported = canRecordVoice();

  let strokes: Point[][] = [];
  let strokeCount = $state(0);
  let drawingBusy = $state(false);
  let activePointer: number | null = null;
  let requestSeq = 0;
  let pendingPick: number | null = null;
  let treeList = $state<HTMLOListElement | null>(null);
  /** Rows below the visible part of the tree: fade its bottom edge. */
  let treeMore = $state(false);
  function updateTreeMore() {
    const list = treeList;
    treeMore = Boolean(list && list.scrollTop + list.clientHeight < list.scrollHeight - 1);
  }

  // Keep the selected row visible when the tree is longer than the sheet.
  $effect(() => {
    void selection?.node;
    const list = treeList;
    if (!list) return;
    // Scroll the list only: scrollIntoView would also move the sheet's
    // ancestors. Show the parent row above the selection when there is one,
    // aligned to a row edge so no row is cut in half at the top.
    queueMicrotask(() => {
      const row = list.querySelector<HTMLElement>(".current");
      if (!row) return;
      const rowTop = row.getBoundingClientRect().top - list.getBoundingClientRect().top + list.scrollTop;
      const rowHeight = row.getBoundingClientRect().height;
      const visible = rowTop >= list.scrollTop && rowTop + rowHeight <= list.scrollTop + list.clientHeight;
      if (!visible) list.scrollTop = Math.max(0, rowTop - rowHeight);
      updateTreeMore();
    });
  });

  /** Browse: the page's text selection, asked for on a timer (the bridge only
   *  answers). Shown in the docked bar; only Add note turns it into a draft. */
  let textSel = $state<TextSelection | null>(null);
  let pendingText: number | null = null;
  /** A finger is on the docked bar: keep it while the tap lands, even if the
   *  page drops its selection when focus leaves the frame. */
  let textHold = false;
  let textHoldTimer: ReturnType<typeof setTimeout> | null = null;
  const TEXT_POLL_MS = 400;

  /** Scroll the answer into view: tree moves can jump outside the visible page. */
  let pendingReveal = false;
  /** Tap mode picks an element first; commenting is a separate, explicit step.
   *  `state`: `gone` (removed from the page) or `hidden` (no box of its own)
   *  selections cannot be commented on. */
  let selection = $state<{
    node: number;
    element: PageElement;
    tree: SelectionTree;
    state: "ok" | "hidden" | "gone";
  } | null>(null);

  /** Asks the page to bring the highlighted element into view once the app's
   *  layout has settled (the sheet or editor shrinks the frame). The bridge
   *  also does this on its own resize; this covers resizes that land first. */
  function revealSoon(node: number) {
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        const current = selection?.node ?? (editorHighlight ? highlightedNode : null);
        if (current === node) toFrame({ type: "highlight", node, reveal: true });
      }),
    );
  }
  const probes = new Map<number, (value: { anchor: PageElement | null; elements: PageElement[]; viewport: PageViewport | null }) => void>();
  let readyTimer: ReturnType<typeof setTimeout> | null = null;
  let hintTimer: ReturnType<typeof setTimeout> | null = null;
  let tapGesture: { id: number; x: number; y: number; lastX: number; lastY: number; scrolling: boolean } | null = null;

  let imageInput = $state<HTMLInputElement | null>(null);

  // Object URLs for blobs shown in the UI. Drafts hold only Blobs, so they can
  // be stored in IndexedDB as they are.
  const objectUrls = new Map<Blob, string>();
  function urlFor(media: Blob): string {
    let url = objectUrls.get(media);
    if (!url) {
      url = URL.createObjectURL(media);
      objectUrls.set(media, url);
    }
    return url;
  }

  /** Update to a newer version: loading it, checking notes against it. */
  let updateState = $state<"idle" | "loading" | "checking">("idle");
  /** Notes are frozen while an update loads, checks, or waits for a choice. */
  const updating = $derived(updateState !== "idle" || panel === "update");
  const canAnnotate = $derived(
    loaded &&
      (isImage ? imageSize !== null : helperReady && !bridgeLost) &&
      stale !== "reopened" &&
      !sending &&
      !updating,
  );
  /** Strokes drawn but not turned into a note yet. */
  const pendingStrokes = $derived(isImage ? imageStrokes.length : strokeCount);

  $effect(() => {
    const open = editing;
    unsaved =
      recording ||
      pendingStrokes > 0 ||
      (open !== null &&
        (open.target.kind === "drawing" ||
          open.target.kind === "image_area" ||
          open.comment.trim() !== "" ||
          open.voice.length > 0 ||
          open.images.length > 0));
  });

  /** The image's marks: every note, and the one being edited highlighted. */
  const stageMarks = $derived.by((): StageMark[] => {
    if (!isImage || stale === "reopened") return [];
    const marks: StageMark[] = [];
    drafts.forEach((draft, index) => {
      if (editing?.id === draft.id) return;
      if (draft.target.kind === "image_pin" || draft.target.kind === "image_area") marks.push({ n: index + 1, target: draft.target });
    });
    const open = editing?.target;
    if (open && (open.kind === "image_pin" || open.kind === "image_area")) marks.push({ n: null, target: open });
    return marks;
  });

  /** Decodes the copy once and checks it shows at the size the server named:
   *  marks are fractions of that frame, so a mismatch is refused, not drawn. */
  async function decodeArtifactImage(source: Blob, expected: Size): Promise<void> {
    if (imageUrl) URL.revokeObjectURL(imageUrl);
    imageUrl = null;
    imageUrl = await decodeImageUrl(source, expected);
    imageSize = { width: expected.width, height: expected.height };
  }

  /** An object URL for `source` once it decodes at exactly `expected`. */
  async function decodeImageUrl(source: Blob, expected: Size): Promise<string> {
    const url = URL.createObjectURL(source);
    const probe = new Image();
    probe.src = url;
    try {
      await probe.decode();
    } catch {
      URL.revokeObjectURL(url);
      throw new Error("This image could not be decoded here.");
    }
    if (probe.naturalWidth !== expected.width || probe.naturalHeight !== expected.height) {
      URL.revokeObjectURL(url);
      throw new Error("The image preview did not show at its expected size, so it cannot be annotated here.");
    }
    return url;
  }

  function buildFrame() {
    if (frameUrl) URL.revokeObjectURL(frameUrl);
    nonce = newNonce();
    helperReady = false;
    bridgeLost = false;
    frameUrl = URL.createObjectURL(new Blob([injectHelper(pageHtml, nonce)], { type: "text/html;charset=utf-8" }));
  }

  // Prepare once per artifact bytes: fingerprint the original, load any saved
  // drafts for this artifact, then serve a copy with the bridge injected.
  $effect(() => {
    const source = blob;
    return untrack(() => {
      let cancelled = false;
      loaded = false;
      void (async () => {
        try {
          // An image's version is the server's hash of the original; the
          // bytes here are its canonical copy.
          imageCopy = image;
          const copy = image;
          const digest = copy ? copy.sourceSha256 : await sha256Hex(source);
          const html = copy ? "" : await source.text();
          recordKey = draftKey(telegramUserId(), threadId, artifact.path);
          const record = await loadDraftRecord(recordKey);
          if (cancelled) return;
          sha = digest;
          if (copy) await decodeArtifactImage(source, copy);
          else pageHtml = html;
          if (cancelled) return;
          if (record && record.drafts.length > 0) {
            drafts = record.drafts;
            draftSha = record.sha;
            draftFrame = record.frame ?? null;
            submissionId = record.submissionId;
            const sameFrame = !copy || record.frame?.canonical_sha256 === copy.canonicalSha256;
            stale = record.sha === digest && sameFrame ? null : "reopened";
            panel = "list";
          } else {
            drafts = [];
            draftSha = digest;
            draftFrame = currentFrame;
          }
          if (!isImage) buildFrame();
          loaded = true;
        } catch (e) {
          if (!cancelled) prepareError = e instanceof Error ? e.message : String(e);
        }
      })();
      return () => {
        cancelled = true;
      };
    });
  });

  function toFrame(message: Record<string, unknown>) {
    // Opaque origin: there is no origin to name. Only geometry, ids, and mode
    // go in; page scripts can read anything sent here.
    try {
      frame?.contentWindow?.postMessage(helperMessage($state.snapshot(message)), "*");
    } catch (e) {
      console.warn("annotation bridge message failed", e);
    }
  }

  function onMessage(event: MessageEvent) {
    if (checker && event.source === checker.contentWindow) {
      onCheckerMessage(event);
      return;
    }
    if (!frame || event.source !== frame.contentWindow) return;
    const parsed = parseHelperEvent(event.data, nonce);
    if (!parsed) return;
    if (parsed.type === "marks-status") {
      unresolved = parsed.unresolved;
      return;
    }
    if (parsed.type === "selection-status") {
      if (selection && parsed.node === selection.node) selection.state = parsed.state;
      return;
    }
    if (parsed.viewport) lastViewport = parsed.viewport;
    if (parsed.type === "ready") {
      helperReady = true;
      bridgeLost = false;
      syncMarks();
    } else if (parsed.type === "picked") {
      // Only the answer to the request the user just made selects anything,
      // and selecting never opens the editor by itself.
      if (parsed.id !== pendingPick || mode !== "select" || panel === "editor" || !canAnnotate) return;
      pendingPick = null;
      if (!parsed.element || parsed.node === null || !parsed.tree) {
        flash("Nothing to select there.");
        return;
      }
      haptic("light");
      selection = { node: parsed.node, element: parsed.element, tree: parsed.tree, state: "ok" };
      panel = "none";
      toFrame({ type: "highlight", node: parsed.node, reveal: pendingReveal });
      revealSoon(parsed.node);
    } else if (parsed.type === "probed") {
      probes.get(parsed.id)?.(parsed);
      probes.delete(parsed.id);
    } else if (parsed.type === "text-selected") {
      if (parsed.id !== pendingText || textHold || !textPolling) return;
      pendingText = null;
      textSel = parsed.selection;
    }
  }

  function onFrameLoad() {
    // Every load (including the page navigating itself away) starts without a
    // bridge until this load's bridge answers.
    helperReady = false;
    selection = null;
    pendingPick = null;
    toFrame({ type: "ping" });
    if (readyTimer) clearTimeout(readyTimer);
    readyTimer = setTimeout(() => {
      if (!helperReady) {
        bridgeLost = true;
        if (mode !== "browse") setMode("browse");
      }
    }, 2000);
  }

  const textPolling = $derived(!isImage && mode === "browse" && canAnnotate && panel !== "editor");

  $effect(() => {
    if (!textPolling) {
      untrack(() => {
        textSel = null;
        pendingText = null;
        textHold = false;
      });
      return;
    }
    const timer = setInterval(() => {
      if (document.visibilityState !== "visible") return;
      pendingText = ++requestSeq;
      toFrame({ type: "text-selection", id: pendingText });
    }, TEXT_POLL_MS);
    return () => clearInterval(timer);
  });

  function holdText() {
    textHold = true;
    if (textHoldTimer) clearTimeout(textHoldTimer);
    textHoldTimer = setTimeout(() => (textHold = false), 1500);
  }

  function addTextNote() {
    const sel = textSel;
    textHold = false;
    if (!sel || !canAnnotate) return;
    textSel = null;
    pendingText = null;
    toFrame({ type: "clear-text" });
    const target = { kind: "text" as const, ...($state.snapshot(sel) as TextSelection) };
    openEditor({ id: newId(), intent: "change", comment: "", target, voice: [], images: [] }, true);
  }

  function syncMarks() {
    toFrame({ type: "marks", marks: stale === "reopened" ? [] : marksFor(drafts) });
  }

  $effect(() => {
    void drafts.length;
    void stale;
    if (helperReady) untrack(syncMarks);
  });

  $effect(() => {
    setClosingConfirmation(drafts.length > 0);
  });

  function flash(text: string) {
    hint = text;
    if (hintTimer) clearTimeout(hintTimer);
    hintTimer = setTimeout(() => (hint = ""), 2500);
  }

  function clearSelection() {
    pendingPick = null;
    if (!selection) return;
    selection = null;
    toFrame({ type: "highlight", node: null });
    // The sheet replaced the notes list; bring it back when there are notes.
    if (panel === "none" && drafts.length > 0) panel = "list";
  }

  // Leaving Tap mode, opening the editor, a send, a stale set, or a lost
  // bridge all end the selection.
  $effect(() => {
    if (mode !== "select" || !canAnnotate || panel === "editor") untrack(clearSelection);
  });

  /** Moves the selection to another element of the tree; the page highlight
   *  follows when the bridge answers. */
  function selectNode(node: TreeNode) {
    if (!selection || !canAnnotate) return;
    pendingPick = ++requestSeq;
    pendingReveal = true;
    toFrame({ type: "select", id: pendingPick, node: node.node });
  }

  /** The page keeps showing the selected element while its note is written. */
  let editorHighlight = false;
  let highlightedNode: number | null = null;

  $effect(() => {
    if (editing === null && editorHighlight) {
      editorHighlight = false;
      toFrame({ type: "highlight", node: null });
    }
  });

  function commentOnSelection() {
    if (!selection || selection.state !== "ok" || !canAnnotate) return;
    const element = selection.element;
    highlightedNode = selection.node;
    pendingPick = null;
    selection = null;
    editorHighlight = true;
    openEditor({ id: newId(), intent: "change", comment: "", target: { kind: "element", element }, voice: [], images: [] }, true);
    revealSoon(highlightedNode);
  }

  /** Notes always does something visible: with a selection open it ends the
   *  selection and shows the list. */
  function toggleNotes() {
    if (selection) {
      clearSelection();
      panel = "list";
      return;
    }
    panel = panel === "list" ? "none" : "list";
  }

  function nodeName(node: TreeNode): string {
    return `<${node.tag}>${node.label ? ` ${node.label}` : ""}`;
  }

  /** A mode change the user asked for. Leaving Draw clears strokes that are
   *  not a note yet, so it asks first; declining keeps them and stays in Draw. */
  async function requestMode(next: Mode) {
    if (mode === "draw" && next !== "draw" && pendingStrokes > 0) {
      if (!(await confirmAction("Discard your unfinished drawing? Tap Comment first to keep it as a note."))) return;
    }
    setMode(next);
  }

  function setMode(next: Mode) {
    if (next !== "browse" && !canAnnotate) return;
    if (mode === "draw" && next !== "draw") clearStrokes();
    if (next !== "browse") hint = "";
    mode = next;
    haptic("light");
  }

  function persist() {
    // Only the latest write decides the status: writes commit in order, so
    // an earlier one finishing says nothing about the current drafts.
    const seq = ++persistSeq;
    storage = "saving";
    void saveDraftRecord({
      key: recordKey,
      sha: draftSha ?? sha ?? "",
      frame: draftFrame ? ($state.snapshot(draftFrame) as ImageFrame) : undefined,
      submissionId,
      drafts: $state.snapshot(drafts) as DraftAnnotation[],
      updatedAt: Date.now(),
    }).then((ok) => {
      if (seq === persistSeq) storage = ok ? "saved" : "failed";
    });
  }

  /** Any change to the drafts makes the next send a new submission. Refused
   *  while a send is in flight. */
  function commit(next: DraftAnnotation[]): boolean {
    if (sending || updating) return false;
    drafts = next;
    submissionId = newId();
    if (next.length === 0) {
      draftSha = sha;
      draftFrame = currentFrame;
      stale = null;
    }
    sendState = "idle";
    sendMessage = "";
    persist();
    return true;
  }

  function openEditor(draft: DraftAnnotation, isNew: boolean) {
    if (sending || updating) return;
    editing = { ...draft, voice: [...draft.voice], images: [...draft.images] };
    editingIsNew = isNew;
    editorError = "";
    panel = "editor";
  }

  function closeEditor() {
    stopRecording();
    editing = null;
    panel = drafts.length > 0 ? "list" : "none";
  }

  function saveEditor() {
    if (!editing || sending) return;
    if (draftIsEmpty(editing)) {
      editorError = "Add a comment, a voice note, or an image, or mark it Keep.";
      return;
    }
    if (editing.comment.length > LIMITS.commentChars) {
      editorError = `Keep the comment under ${LIMITS.commentChars} characters.`;
      return;
    }
    stopRecording();
    const saved = $state.snapshot(editing) as DraftAnnotation;
    const index = drafts.findIndex((d) => d.id === saved.id);
    if (index === -1) {
      if (drafts.length >= LIMITS.annotations) {
        editorError = `You can send up to ${LIMITS.annotations} notes at once.`;
        return;
      }
      commit([...drafts, saved]);
    } else {
      commit(drafts.map((d) => (d.id === saved.id ? saved : d)));
    }
    editing = null;
    panel = "list";
    haptic("light");
  }

  function deleteDraft(id: string) {
    if (!commit(drafts.filter((d) => d.id !== id))) return;
    if (editing?.id === id) editing = null;
    if (panel === "editor" && !editing) panel = drafts.length > 0 ? "list" : "none";
  }

  function discardAll() {
    if (!commit([])) return;
    editing = null;
    panel = "none";
  }

  /** A tap on the image: a new pin note, or moves the pin being edited. */
  function onImagePin(point: ImagePoint) {
    if (!canAnnotate) return;
    haptic("light");
    if (panel === "editor") {
      if (editing?.target.kind === "image_pin") editing.target = { kind: "image_pin", point };
      return;
    }
    openEditor({ id: newId(), intent: "change", comment: "", target: { kind: "image_pin", point }, voice: [], images: [] }, true);
  }

  function addPageNote() {
    openEditor({ id: newId(), intent: "note", comment: "", target: { kind: "page" }, voice: [], images: [] }, true);
  }

  // --- tap mode: the parent owns input, so page handlers and links never fire ---

  function stagePoint(event: PointerEvent | WheelEvent): Point {
    const rect = stage!.getBoundingClientRect();
    return [event.clientX - rect.left, event.clientY - rect.top];
  }

  function onTapDown(event: PointerEvent) {
    if (tapGesture || panel === "editor") return;
    event.preventDefault();
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    const [x, y] = stagePoint(event);
    tapGesture = { id: event.pointerId, x, y, lastX: x, lastY: y, scrolling: false };
  }

  function onTapMove(event: PointerEvent) {
    if (!tapGesture || event.pointerId !== tapGesture.id) return;
    event.preventDefault();
    const [x, y] = stagePoint(event);
    if (!tapGesture.scrolling && Math.hypot(x - tapGesture.x, y - tapGesture.y) > 8) tapGesture.scrolling = true;
    if (tapGesture.scrolling) {
      toFrame({ type: "scroll", x: tapGesture.x, y: tapGesture.y, dx: tapGesture.lastX - x, dy: tapGesture.lastY - y });
      tapGesture.lastX = x;
      tapGesture.lastY = y;
    }
  }

  function onTapUp(event: PointerEvent) {
    if (!tapGesture || event.pointerId !== tapGesture.id) return;
    const gesture = tapGesture;
    tapGesture = null;
    if (gesture.scrolling || event.type === "pointercancel") return;
    pendingPick = ++requestSeq;
    pendingReveal = false;
    toFrame({ type: "pick", id: pendingPick, x: gesture.x, y: gesture.y });
  }

  function onStageWheel(event: WheelEvent) {
    event.preventDefault();
    const [x, y] = stagePoint(event);
    toFrame({ type: "scroll", x, y, dx: event.deltaX, dy: event.deltaY });
  }

  // --- drawing ---

  function resizeCanvas() {
    if (!canvas || !stage) return;
    const ratio = Math.min(window.devicePixelRatio || 1, 3);
    canvas.width = Math.round(stage.clientWidth * ratio);
    canvas.height = Math.round(stage.clientHeight * ratio);
    redraw();
  }

  function redraw() {
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const ratio = canvas.width / Math.max(1, canvas.clientWidth);
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, canvas.clientWidth, canvas.clientHeight);
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
  }

  $effect(() => {
    if (mode === "draw" && canvas) untrack(resizeCanvas);
  });

  function point(event: PointerEvent): Point {
    const rect = canvas!.getBoundingClientRect();
    return [Math.round((event.clientX - rect.left) * 10) / 10, Math.round((event.clientY - rect.top) * 10) / 10];
  }

  function onPointerDown(event: PointerEvent) {
    if (!canvas || activePointer !== null || drawingBusy || panel === "editor") return;
    event.preventDefault();
    activePointer = event.pointerId;
    canvas.setPointerCapture(event.pointerId);
    strokes.push([point(event)]);
    strokeCount = strokes.length;
    redraw();
  }

  function onPointerMove(event: PointerEvent) {
    if (event.pointerId !== activePointer) return;
    event.preventDefault();
    const stroke = strokes[strokes.length - 1];
    const next = point(event);
    const last = stroke[stroke.length - 1];
    if (Math.hypot(next[0] - last[0], next[1] - last[1]) < 1.5) return;
    if (stroke.length < 2000) stroke.push(next);
    redraw();
  }

  function onPointerUp(event: PointerEvent) {
    if (event.pointerId !== activePointer) return;
    activePointer = null;
  }

  function clearStrokes() {
    imageStrokes = [];
    strokes = [];
    strokeCount = 0;
    activePointer = null;
    redraw();
  }

  function undoStroke() {
    if (isImage) {
      imageStrokes = imageStrokes.slice(0, -1);
      return;
    }
    strokes.pop();
    strokeCount = strokes.length;
    redraw();
  }

  function probe(points: Point[]): Promise<{ anchor: PageElement | null; elements: PageElement[]; viewport: PageViewport | null }> {
    const id = ++requestSeq;
    return new Promise((resolve) => {
      probes.set(id, resolve);
      toFrame({ type: "probe", id, points });
      setTimeout(() => {
        if (probes.delete(id)) resolve({ anchor: null, elements: [], viewport: null });
      }, 1500);
    });
  }

  async function finishDrawing() {
    if (isImage) {
      if (imageStrokes.length === 0) return;
      const drawn = limitArea($state.snapshot(imageStrokes) as ImagePoint[][]);
      imageStrokes = [];
      openEditor({ id: newId(), intent: "change", comment: "", target: { kind: "image_area", strokes: drawn }, voice: [], images: [] }, true);
      return;
    }
    const bounds = strokeBounds(strokes);
    if (!bounds || !stage) return;
    drawingBusy = true;
    try {
      const drawn = strokes.map((s) => [...s]);
      const result = await probe(samplePoints(drawn));
      if (!result.viewport) {
        flash("The page did not answer. Try again, or reload it.");
        return;
      }
      const viewport = result.viewport;
      const sketch = await renderSketch(drawn, result.elements, viewport);
      const target: DraftTarget = {
        kind: "drawing",
        bounds: { ...bounds, x: bounds.x + viewport.scroll_x, y: bounds.y + viewport.scroll_y },
        anchor: result.anchor,
        offset: result.anchor ? anchorOffset(bounds, result.anchor, viewport) : null,
        elements: result.elements,
        strokes: drawn,
        viewport,
        sketch,
      };
      clearStrokes();
      openEditor({ id: newId(), intent: "change", comment: "", target, voice: [], images: [] }, true);
    } catch (e) {
      flash(e instanceof Error ? e.message : String(e));
    } finally {
      drawingBusy = false;
    }
  }

  // --- media ---

  async function startRecording() {
    if (!editing || recording) return;
    editorError = "";
    if (editing.voice.length + editing.images.length >= LIMITS.mediaPerAnnotation) {
      editorError = `Up to ${LIMITS.mediaPerAnnotation} attachments per note.`;
      return;
    }
    try {
      recordStream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (e) {
      editorError =
        e instanceof DOMException && e.name === "NotAllowedError"
          ? "Microphone access was denied."
          : "The microphone is not available here.";
      return;
    }
    const mime = pickRecorderMime();
    const chunks: Blob[] = [];
    recorder = mime ? new MediaRecorder(recordStream, { mimeType: mime }) : new MediaRecorder(recordStream);
    const started = Date.now();
    recorder.ondataavailable = (event) => {
      if (event.data.size > 0) chunks.push(event.data);
    };
    recorder.onstop = () => {
      const type = (recorder?.mimeType || mime || "audio/webm").split(";")[0];
      const clip = new Blob(chunks, { type });
      releaseMic();
      recording = false;
      if (!editing || clip.size === 0) return;
      if (clip.size > LIMITS.audioBytes) {
        editorError = "That voice note is too long. Keep it under a few minutes.";
        return;
      }
      editing.voice = [...editing.voice, { blob: clip, durationMs: Date.now() - started }];
    };
    recorder.start(1000);
    recording = true;
    recordMs = 0;
    recordTimer = setInterval(() => {
      recordMs = Date.now() - started;
      if (recordMs >= LIMITS.voiceMs) stopRecording();
    }, 250);
    haptic("medium");
  }

  function stopRecording() {
    if (recordTimer) clearInterval(recordTimer);
    recordTimer = null;
    if (recorder && recorder.state !== "inactive") recorder.stop();
    else releaseMic();
  }

  function releaseMic() {
    for (const track of recordStream?.getTracks() ?? []) track.stop();
    recordStream = null;
    recorder = null;
  }

  async function onImagePicked(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (!file || !editing) return;
    editorError = "";
    if (editing.voice.length + editing.images.length >= LIMITS.mediaPerAnnotation) {
      editorError = `Up to ${LIMITS.mediaPerAnnotation} attachments per note.`;
      return;
    }
    try {
      const prepared = await prepareImage(file);
      if (prepared.size > LIMITS.imageBytes) {
        editorError = "That image is too large.";
        return;
      }
      if (editing) editing.images = [...editing.images, { blob: prepared }];
    } catch {
      editorError = "That file could not be read as an image.";
    }
  }

  function removeVoice(index: number) {
    if (editing) editing.voice = editing.voice.filter((_, i) => i !== index);
  }

  function removeImage(index: number) {
    if (editing) editing.images = editing.images.filter((_, i) => i !== index);
  }

  // --- a newer version of the file ---

  /** How often an open, visible viewer asks the bot host for the file's
   *  version. A local read of the bot's file, never a Telegram call. */
  const VERSION_POLL_MS = 30_000;
  /** Focus and visibility events closer than this share one check. */
  const VERSION_MIN_GAP_MS = 10_000;
  /** How long the new page may take to load (its load event and web fonts),
   *  and then to answer each request. Past it, notes do not move. */
  const CHECK_LOAD_TIMEOUT_MS = 8_000;
  const CHECK_TIMEOUT_MS = 5_000;

  /** A newer version, loaded but not shown. `sha` names exactly these bytes
   *  (hashed here for HTML; the server's source hash for an image), never the
   *  hash the version check advertised. */
  type Candidate = { sha: string; html: string | null; copy: ArtifactImageCopy | null; imageUrl: string | null };

  /** The file's version on the bot host at the last check. */
  let advertised = $state<ArtifactVersion | null>(null);
  const newVersion = $derived(loaded && sha !== null && advertised !== null && advertised.sha256 !== sha);
  let updateError = $state("");
  let updateInfo = $state("");
  /** Some notes could not be matched: the choice is open in the `update` panel. */
  let updateResult = $state<{ candidate: Candidate; checks: NoteCheck[] } | null>(null);
  /** Work an update must not interrupt. It waits; nothing is discarded. */
  const updateBlock = $derived(
    sending
      ? "Wait for the send to finish."
      : panel === "editor" || recording
        ? "Save or cancel the note you are editing first."
        : pendingStrokes > 0
          ? "Turn your drawing into a note or clear it first."
          : "",
  );
  let versionInFlight = false;
  let lastVersionCheck = 0;
  /** The preview closed while an update was loading: show nothing. */
  let destroyed = false;

  async function checkVersion() {
    const now = Date.now();
    if (versionInFlight || now - lastVersionCheck < VERSION_MIN_GAP_MS) return;
    versionInFlight = true;
    lastVersionCheck = now;
    try {
      advertised = await api.artifactVersion(threadId, artifact.path);
    } catch {
      // Gone, offline, or expired: keep what is shown; the next check retries.
    } finally {
      versionInFlight = false;
    }
  }

  // Only while the viewer is open and on screen; never a reload by itself.
  $effect(() => {
    if (!loaded || prepareError) return;
    const visible = () => {
      if (document.visibilityState === "visible") void checkVersion();
    };
    const timer = setInterval(visible, VERSION_POLL_MS);
    document.addEventListener("visibilitychange", visible);
    window.addEventListener("focus", visible);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", visible);
      window.removeEventListener("focus", visible);
    };
  });

  // The new page is checked in a hidden frame of the same size, with its own
  // bridge, while the page on screen stays as it is.
  let checker = $state<HTMLIFrameElement | null>(null);
  let checkerUrl = $state<string | null>(null);
  let checkerNonce = "";
  /** The id of the `settle` request sent after the frame's load event. */
  let checkerSettleId: number | null = null;
  let checkerReady: (() => void) | null = null;
  const checkerAnswers = new Map<number, (answer: { items: InspectedItem[]; viewport: PageViewport | null }) => void>();
  const checkerRects = new Map<number, (rects: (MeasuredRange | null)[]) => void>();

  /** The bridge posts `ready` as soon as it runs, before the body, styles,
   *  scripts, or fonts are there. The checker ignores it: it is ready only on
   *  the answer to the `settle` it sends after the frame's load event. */
  function onCheckerMessage(event: MessageEvent) {
    const parsed = parseHelperEvent(event.data, checkerNonce);
    if (parsed?.type === "settled" && parsed.id === checkerSettleId) checkerReady?.();
    else if (parsed?.type === "inspected") checkerAnswers.get(parsed.id)?.(parsed);
    else if (parsed?.type === "range-rects") checkerRects.get(parsed.id)?.(parsed.rects);
  }

  function onCheckerLoad() {
    checkerSettleId = ++requestSeq;
    try {
      checker?.contentWindow?.postMessage(helperMessage({ type: "settle", id: checkerSettleId }), "*");
    } catch {
      // The timeout reports a page that does not answer.
    }
  }

  function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T | null> {
    return new Promise((resolve) => {
      const timer = setTimeout(() => resolve(null), ms);
      promise.then(
        (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        () => {
          clearTimeout(timer);
          resolve(null);
        },
      );
    });
  }

  /** Asks the new page about the notes' targets, then where each text
   *  note's matched words are. `null` when it does not load its bridge or
   *  answer in time; a range it does not measure stays out of `measured`.
   *  Only selectors and character offsets go in; nothing the user wrote. */
  async function inspectNewPage(html: string, plan: InspectPlan, notes: DraftAnnotation[]) {
    checkerNonce = newNonce();
    checkerSettleId = null;
    const ready = new Promise<boolean>((resolve) => (checkerReady = () => resolve(true)));
    checkerUrl = URL.createObjectURL(new Blob([injectHelper(html, checkerNonce)], { type: "text/html;charset=utf-8" }));
    try {
      if ((await withTimeout(ready, CHECK_LOAD_TIMEOUT_MS)) === null) return null;
      const id = ++requestSeq;
      const answer = new Promise<{ items: InspectedItem[]; viewport: PageViewport | null }>((resolve) =>
        checkerAnswers.set(id, resolve),
      );
      checker?.contentWindow?.postMessage(helperMessage({ type: "inspect", id, items: plan.items }), "*");
      const inspected = await withTimeout(answer, CHECK_TIMEOUT_MS);
      if (!inspected) return null;
      const measured = new Map<number, Rect | null>();
      const ranges = rangeRequests(notes, plan, inspected.items);
      if (ranges.length > 0) {
        const rectsId = ++requestSeq;
        const rects = new Promise<(MeasuredRange | null)[]>((resolve) => checkerRects.set(rectsId, resolve));
        const items = ranges.map(({ selector, start, end }) => ({ selector, start, end }));
        checker?.contentWindow?.postMessage(helperMessage({ type: "range-rects", id: rectsId, items }), "*");
        const boxes = await withTimeout(rects, CHECK_TIMEOUT_MS);
        for (const [note, box] of acceptMeasured(notes, ranges, boxes)) measured.set(note, box);
      }
      return { ...inspected, measured };
    } finally {
      checkerReady = null;
      checkerSettleId = null;
      checkerAnswers.clear();
      checkerRects.clear();
      if (checkerUrl) URL.revokeObjectURL(checkerUrl);
      checkerUrl = null;
    }
  }

  async function fetchCandidate(): Promise<Candidate> {
    if (isImage) {
      let copy: ArtifactImageCopy;
      try {
        copy = await api.artifactImage(threadId, artifact.path);
      } catch (e) {
        if (e instanceof ApiError && e.status === 422) throw new Error(`the new version cannot be annotated: ${e.message}`);
        throw e;
      }
      return { sha: copy.sourceSha256, html: null, copy, imageUrl: await decodeImageUrl(copy.blob, copy) };
    }
    const blob = await api.artifactBlob(threadId, artifact.path);
    return { sha: await sha256Hex(blob), html: await blob.text(), copy: null, imageUrl: null };
  }

  function dropCandidate(candidate: Candidate) {
    if (candidate.imageUrl) URL.revokeObjectURL(candidate.imageUrl);
  }

  /** Shows `candidate`. With `moved`, the notes go with it; without, notes
   *  keep the version they were made on (marked earlier, never moved). */
  function applyCandidate(candidate: Candidate, moved: DraftAnnotation[] | null) {
    sha = candidate.sha;
    if (candidate.copy) {
      if (imageUrl) URL.revokeObjectURL(imageUrl);
      imageUrl = candidate.imageUrl;
      imageSize = { width: candidate.copy.width, height: candidate.copy.height };
      imageCopy = candidate.copy;
      imageStrokes = [];
    } else {
      pageHtml = candidate.html ?? "";
      buildFrame();
    }
    if (moved) {
      draftSha = candidate.sha;
      draftFrame = currentFrame;
      drafts = moved;
      submissionId = newId();
      stale = null;
      sendState = "idle";
      sendMessage = "";
      persist();
      updateInfo = `Updated. ${moved.length === 1 ? "Your note was" : `All ${moved.length} notes were`} found unchanged in the new version and moved to it. The check compares selectors, text, markup, and sizes, so look ${moved.length === 1 ? "it" : "them"} over before sending.`;
    } else if (drafts.length === 0) {
      draftSha = candidate.sha;
      draftFrame = currentFrame;
      stale = null;
      updateInfo = "Updated to the new version.";
    } else {
      const sameFrame = !isImage || draftFrame?.canonical_sha256 === currentFrame?.canonical_sha256;
      stale = draftSha === candidate.sha && sameFrame ? null : "reopened";
      updateInfo = stale
        ? "Updated. Your notes stay as notes on the earlier version."
        : "Updated. This is the version your notes were made on, so they apply again.";
    }
    updateResult = null;
    if (panel === "update" || panel === "none") panel = drafts.length > 0 ? "list" : "none";
  }

  async function startUpdate() {
    if (!newVersion || updateBlock || updateState !== "idle") return;
    updateError = "";
    updateInfo = "";
    clearSelection();
    if (mode !== "browse") setMode("browse");
    updateState = "loading";
    let candidate: Candidate | null = null;
    try {
      candidate = await fetchCandidate();
      if (destroyed) return;
      if (candidate.sha === sha) {
        // Changed back to what is shown.
        dropCandidate(candidate);
        candidate = null;
        advertised = null;
        updateInfo = "The file is the same as the version you have open.";
        return;
      }
      const notes = $state.snapshot(drafts) as DraftAnnotation[];
      // Notes already on an earlier version keep it: checking them against
      // what is shown would match them to the wrong version.
      if (notes.length === 0 || stale === "reopened") {
        applyCandidate(candidate, null);
        candidate = null;
        return;
      }
      updateState = "checking";
      let checks: NoteCheck[];
      if (candidate.copy) {
        checks = checkImageNotes(notes, candidate.copy.canonicalSha256 === draftFrame?.canonical_sha256);
      } else {
        const plan = inspectPlan(notes);
        const answer =
          plan.items.length > 0
            ? await inspectNewPage(candidate.html ?? "", plan, notes)
            : { items: [], viewport: null, measured: new Map<number, Rect | null>() };
        checks = checkHtmlNotes(notes, plan, answer?.items ?? null, answer?.viewport ?? null, answer?.measured);
      }
      if (destroyed) return;
      if (allMatched(checks)) {
        applyCandidate(candidate, carryOver(notes, checks, sha!));
      } else {
        updateResult = { candidate, checks };
        panel = "update";
      }
      candidate = null;
    } catch (e) {
      const text = e instanceof Error ? e.message : String(e);
      updateError = `Could not load the new version (${text}). You are still on the version you opened.`;
    } finally {
      if (candidate) dropCandidate(candidate);
      updateState = "idle";
    }
  }

  function stayOnVersion() {
    if (updateResult) dropCandidate(updateResult.candidate);
    updateResult = null;
    panel = drafts.length > 0 ? "list" : "none";
  }

  function updateAnyway() {
    if (updateResult) applyCandidate(updateResult.candidate, null);
  }

  // --- send ---

  async function send(allowStale = false) {
    // Unfinished strokes would be cleared by leaving Draw: they are not sent
    // and must not vanish silently.
    if (!draftSha || drafts.length === 0 || sendState === "sending" || panel === "editor" || pendingStrokes > 0) return;
    if (updating) return;
    sendState = "sending";
    sendMessage = "";
    if (mode !== "browse") setMode("browse");
    const sentDrafts = $state.snapshot(drafts) as DraftAnnotation[];
    try {
      const payload = await buildPayload({
        submissionId,
        artifactPath: artifact.path,
        sha256: draftSha,
        allowStale: allowStale || stale !== null,
        viewport: isImage ? null : lastViewport,
        image: draftFrame ? ($state.snapshot(draftFrame) as ImageFrame) : null,
        drafts: sentDrafts,
      });
      const result = await api.sendArtifactFeedback(threadId, payload);
      // Remove exactly what was sent. Editing is frozen during a send, so this
      // is normally everything; anything else keeps its notes and gets a new id.
      const sentIds = new Set(sentDrafts.map((d) => d.id));
      drafts = drafts.filter((d) => !sentIds.has(d.id));
      if (drafts.length === 0) {
        draftSha = sha;
        draftFrame = currentFrame;
        stale = null;
      }
      submissionId = newId();
      persist();
      panel = drafts.length > 0 ? "list" : "none";
      sendState = "sent";
      sendMessage =
        result.status === "duplicate"
          ? "Already queued earlier. Nothing was sent twice."
          : "Queued in the chat. The agent picks it up after any turn that is running; a bot restart before then loses it.";
      notify("success");
    } catch (e) {
      sendState = "error";
      if (e instanceof ApiError && e.status === 412) {
        stale = "server";
        sendMessage = "";
      } else {
        const text = e instanceof Error ? e.message : String(e);
        sendMessage =
          storage === "saved"
            ? `${text} Your notes are saved on this device.`
            : storage === "saving"
              ? `${text} Your notes are still being saved on this device.`
              : `${text} Your notes could not be saved on this device; keep the preview open to retry.`;
      }
      notify("error");
    }
  }

  function formatClock(ms: number): string {
    const s = Math.floor(ms / 1000);
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  }

  function intentLabel(value: string): string {
    return INTENTS.find((i) => i.value === value)?.label ?? value;
  }

  onMount(() => {
    window.addEventListener("message", onMessage);
    window.addEventListener("resize", resizeCanvas);
  });

  onDestroy(() => {
    destroyed = true;
    window.removeEventListener("message", onMessage);
    window.removeEventListener("resize", resizeCanvas);
    if (readyTimer) clearTimeout(readyTimer);
    if (hintTimer) clearTimeout(hintTimer);
    if (textHoldTimer) clearTimeout(textHoldTimer);
    if (checkerUrl) URL.revokeObjectURL(checkerUrl);
    if (updateResult) dropCandidate(updateResult.candidate);
    stopRecording();
    setClosingConfirmation(false);
    if (frameUrl) URL.revokeObjectURL(frameUrl);
    if (imageUrl) URL.revokeObjectURL(imageUrl);
    for (const url of objectUrls.values()) URL.revokeObjectURL(url);
  });
</script>

<div class="annot-root">
  <div class="annot-toolbar" role="toolbar" aria-label="Annotation mode">
    <div class="mode-group">
      <button type="button" class="mode-btn" aria-pressed={mode === "browse"} onclick={() => requestMode("browse")}>
        <Icon name="hand" />
        <span>Browse</span>
      </button>
      <button
        type="button"
        class="mode-btn"
        aria-pressed={mode === "select"}
        disabled={!canAnnotate}
        title={isImage ? "Tap the image to pin a spot" : "Tap an element to comment on it"}
        onclick={() => requestMode("select")}
      >
        <Icon name="pointer" />
        <span>Tap</span>
      </button>
      <button type="button" class="mode-btn" aria-pressed={mode === "draw"} disabled={!canAnnotate} onclick={() => requestMode("draw")}>
        <Icon name="pencil" />
        <span>Draw</span>
      </button>
    </div>
    <button
      type="button"
      class="notes-btn"
      aria-pressed={panel === "list" && !selection}
      onclick={toggleNotes}
      disabled={panel === "editor" || updating}
    >
      <Icon name="list" />
      <span>Notes</span>
      {#if drafts.length > 0}<span class="count">{drafts.length}</span>{/if}
    </button>
  </div>

  {#if imageCopy && (imageCopy.sourceWidth > imageCopy.width || imageCopy.sourceHeight > imageCopy.height)}
    <p class="copy-note">
      Optimized copy for annotation: {imageCopy.width}×{imageCopy.height} of {imageCopy.sourceWidth}×{imageCopy.sourceHeight}
      px.
      The original is unchanged; the agent gets this copy with your marks.
    </p>
  {/if}

  {#if (newVersion || updateState !== "idle") && panel !== "update"}
    <div class="banner info" role="status" data-new-version>
      <p class="m-0 min-w-0 flex-1">
        <b class="text-foreground">New version available.</b>
        {#if updateState === "loading"}
          Loading it…
        {:else if updateState === "checking"}
          Checking your notes against it…
        {:else if updateError}
          {updateError}
        {:else if updateBlock}
          {updateBlock}
        {:else if drafts.length > 0 && stale !== "reopened"}
          Your notes are checked against it before anything changes.
        {:else}
          Nothing reloads until you tap Update.
        {/if}
      </p>
      <button
        type="button"
        class="act-btn primary"
        onclick={startUpdate}
        disabled={!!updateBlock || updateState !== "idle"}
        data-update
      >
        Update
      </button>
    </div>
  {:else if updateInfo}
    <div class="banner info" role="status" data-update-info>
      <p class="m-0 min-w-0 flex-1">{updateInfo}</p>
      <button type="button" class="icon-btn" aria-label="Dismiss" onclick={() => (updateInfo = "")}>
        <Icon name="x" />
      </button>
    </div>
  {/if}

  {#if bridgeLost}
    <div class="banner" role="status">
      <p class="m-0 min-w-0 flex-1">
        Annotation is not running on this page: it navigated away from the artifact or blocked scripts. You can
        still add a note on the whole page.
      </p>
      <button type="button" class="act-btn" onclick={buildFrame}>Reload artifact</button>
    </div>
  {:else if stale}
    <div class="banner warn" role="status">
      <p class="m-0 min-w-0 flex-1">
        {#if stale === "reopened"}
          {drafts.length === 1 ? "This note was" : `These ${drafts.length} notes were`} made on an earlier version of
          this file, so {drafts.length === 1 ? "its mark is" : "their marks are"} hidden.
        {:else}
          The file changed on the bot host after you opened it. Your notes describe the version you saw.
        {/if}
      </p>
      <button type="button" class="act-btn primary" onclick={() => send(true)} disabled={sending || pendingStrokes > 0}>
        Send anyway
      </button>
      <button type="button" class="act-btn" onclick={discardAll} disabled={sending}>Discard</button>
    </div>
  {/if}

  <div class="annot-stage" class:image-stage={isImage} bind:this={stage}>
    {#if prepareError}
      <p class="m-3 rounded-md border border-destructive px-3 py-2 text-xs text-destructive">{prepareError}</p>
    {:else if isImage}
      {#if imageUrl && imageSize}
        <ImageStage
          src={imageUrl}
          alt={artifact.name}
          natural={imageSize}
          {mode}
          tapEnabled={canAnnotate && (panel !== "editor" || editing?.target.kind === "image_pin")}
          drawEnabled={canAnnotate && panel !== "editor"}
          marks={stageMarks}
          focus={editing?.target.kind === "image_pin" || editing?.target.kind === "image_area" ? editing.target : null}
          bind:strokes={imageStrokes}
          onpin={onImagePin}
          onhint={flash}
        />
      {/if}
      {#if mode === "select" && (hint || panel !== "editor")}
        <div class="stage-bar passive">
          <span class="stage-hint">{hint || "Tap the image to pin a spot. Pinch to zoom, drag to pan."}</span>
        </div>
      {:else if mode === "browse" && hint}
        <div class="stage-bar passive"><span class="stage-hint">{hint}</span></div>
      {/if}
    {:else if frameUrl}
      <!-- Opaque origin: no `allow-same-origin`, so the document gets no
           access to app storage, cookies or Telegram credentials. The bytes
           arrive over an authenticated fetch and the blob URL itself carries
           no auth, which is what makes this safe to hand to a scripted page.
           `allow-scripts` stays so dashboards and reports still run, and so
           the injected annotation bridge can answer. -->
      <iframe
        bind:this={frame}
        title={artifact.name}
        src={frameUrl}
        sandbox="allow-scripts"
        class="annot-frame"
        onload={onFrameLoad}
      ></iframe>
      {#if checkerUrl}
        <!-- The new version, checked out of sight; the page above stays. -->
        <iframe
          bind:this={checker}
          title="New version check"
          src={checkerUrl}
          sandbox="allow-scripts"
          class="annot-frame checker-frame"
          aria-hidden="true"
          tabindex="-1"
          onload={onCheckerLoad}
        ></iframe>
      {/if}
    {/if}
    {#if !isImage && mode === "browse" && textSel}
      <div
        class="text-dock"
        role="group"
        aria-label="Selected text"
        onpointerdown={holdText}
        data-text-dock
      >
        <div class="min-w-0 flex-1">
          <small>Selected text</small>
          <span class="dock-quote">“{quotePreview(textSel.quote, 140)}”</span>
        </div>
        <button type="button" class="stage-btn primary" onclick={addTextNote}>
          <Icon name="message-square" /><span>Add note</span>
        </button>
      </div>
    {/if}
    {#if !isImage && mode === "select"}
      <div
        class="annot-overlay"
        role="presentation"
        onpointerdown={onTapDown}
        onpointermove={onTapMove}
        onpointerup={onTapUp}
        onpointercancel={onTapUp}
        onwheel={onStageWheel}
      ></div>
      {#if hint || (!selection && panel !== "editor")}
        <div class="stage-bar passive">
          <span class="stage-hint">{hint || "Tap an element to select it. Drag to scroll."}</span>
        </div>
      {/if}
    {/if}
    {#if !isImage && mode === "draw"}
      <canvas
        bind:this={canvas}
        class="annot-canvas"
        aria-label="Drawing layer"
        onpointerdown={onPointerDown}
        onpointermove={onPointerMove}
        onpointerup={onPointerUp}
        onpointercancel={onPointerUp}
      ></canvas>
    {/if}
    {#if mode === "draw" && !prepareError}
      <div class="stage-bar">
        {#if hint}
          <span class="stage-hint">{hint}</span>
        {:else if pendingStrokes === 0}
          <span class="stage-hint">
            {isImage ? "Draw on the image. Pinch to zoom." : "Draw on the page. Switch to Browse to scroll."}
          </span>
        {:else}
          <button type="button" class="stage-btn" onclick={undoStroke} disabled={drawingBusy}>
            <Icon name="undo" /><span>Undo</span>
          </button>
          <button type="button" class="stage-btn" onclick={clearStrokes} disabled={drawingBusy}>
            <Icon name="x" /><span>Clear</span>
          </button>
          <button type="button" class="stage-btn primary" onclick={finishDrawing} disabled={drawingBusy}>
            <span>{drawingBusy ? "Reading page…" : "Comment"}</span>
          </button>
        {/if}
      </div>
    {/if}
  </div>

  {#if panel === "editor" && editing}
    <section class="panel" aria-label="Edit note">
      <div class="panel-head">
        <p class="m-0 min-w-0 flex-1 truncate text-xs font-semibold">
          {editing.target.kind === "text" ? "Selected text" : targetSummary(editing.target, surface)}
        </p>
        <button type="button" class="icon-btn" aria-label="Cancel note" onclick={closeEditor}><Icon name="x" /></button>
      </div>
      {#if editing.target.kind === "text"}
        <blockquote class="quote-block">{quotePreview(editing.target.quote, 400)}</blockquote>
      {:else if editing.target.kind === "drawing"}
        <img class="sketch" src={urlFor(editing.target.sketch)} alt="Your drawing over outlines of the page" />
      {:else if editing.target.kind === "image_pin"}
        <p class="m-0 text-xxs text-muted-foreground">
          {mode === "select" ? "Tap the image to move the pin." : "Switch to Tap to move the pin."} The agent gets the
          image with your marks drawn on it.
        </p>
      {:else if editing.target.kind === "image_area"}
        <p class="m-0 text-xxs text-muted-foreground">The agent gets the image with your marks drawn on it.</p>
      {/if}
      {#if targetDrawsPixels(editing.target)}
        <p class="m-0 text-xxs text-muted-foreground">
          This part is drawn as pixels the agent cannot see. Attach a screenshot if the picture matters.
        </p>
      {/if}
      <div class="chips" role="radiogroup" aria-label="What should happen">
        {#each INTENTS as intent (intent.value)}
          <button
            type="button"
            role="radio"
            aria-checked={editing.intent === intent.value}
            class="chip"
            onclick={() => editing && (editing.intent = intent.value)}
          >
            {intent.label}
          </button>
        {/each}
      </div>
      <textarea
        class="comment"
        rows="3"
        maxlength={LIMITS.commentChars}
        placeholder={editing.intent === "keep" ? "Optional: why keep this?" : "What should change?"}
        bind:value={editing.comment}
      ></textarea>
      {#if editing.voice.length > 0 || editing.images.length > 0}
        <div class="media">
          {#each editing.voice as clip, index (clip.blob)}
            <div class="media-row">
              <audio controls src={urlFor(clip.blob)} class="min-w-0 flex-1"></audio>
              <button type="button" class="icon-btn" aria-label="Remove voice note" onclick={() => removeVoice(index)}>
                <Icon name="trash" />
              </button>
            </div>
          {/each}
          {#if editing.images.length > 0}
            <div class="thumbs">
              {#each editing.images as image, index (image.blob)}
                <div class="thumb">
                  <img src={urlFor(image.blob)} alt="Attached" />
                  <button type="button" class="thumb-remove" aria-label="Remove image" onclick={() => removeImage(index)}>
                    <Icon name="x" />
                  </button>
                </div>
              {/each}
            </div>
          {/if}
        </div>
      {/if}
      {#if editorError}<p class="m-0 text-xs text-destructive">{editorError}</p>{/if}
      <div class="panel-actions">
        {#if voiceSupported}
          {#if recording}
            <button type="button" class="act-btn recording" onclick={stopRecording}>
              <Icon name="square" /><span>Stop {formatClock(recordMs)}</span>
            </button>
          {:else}
            <button type="button" class="act-btn" onclick={startRecording}>
              <Icon name="mic" /><span>Voice</span>
            </button>
          {/if}
        {/if}
        <button type="button" class="act-btn" onclick={() => imageInput?.click()} disabled={recording}>
          <Icon name="image" /><span>Image</span>
        </button>
        <input bind:this={imageInput} type="file" accept="image/*" class="hidden" onchange={onImagePicked} />
        <span class="flex-1"></span>
        {#if !editingIsNew}
          <button type="button" class="icon-btn danger" aria-label="Delete note" onclick={() => editing && deleteDraft(editing.id)}>
            <Icon name="trash" />
          </button>
        {/if}
        <button type="button" class="act-btn primary" onclick={saveEditor} disabled={recording}>
          <span>{editingIsNew ? "Add note" : "Save"}</span>
        </button>
      </div>
    </section>
  {:else if panel === "update" && updateResult}
    {@const checks = updateResult.checks}
    {@const matched = checks.filter((check) => check.ok).length}
    <section class="panel" aria-label="New version" data-update-sheet>
      <p class="m-0 text-xs font-semibold">
        {matched} of {checks.length} notes still match the new version.
      </p>
      <ol class="note-list">
        {#each drafts as draft, index (draft.id)}
          {@const check = checks[index]}
          {#if check && !check.ok}
            <li class="note-row">
              <span class="note-main">
                <span class="note-n">{index + 1}</span>
                <span class="min-w-0 flex-1">
                  <span class="block truncate text-xs">
                    <span class="intent intent-{draft.intent}">{intentLabel(draft.intent)}</span>
                    {targetSummary(draft.target, surface)}
                  </span>
                  <span class="block text-xxs text-warning">Not matched: {check.reason}</span>
                </span>
              </span>
            </li>
          {/if}
        {/each}
      </ol>
      <p class="m-0 text-xxs text-muted-foreground">
        Stay on this version keeps the {surface} you have open and your notes as they are. Update anyway opens the new
        version and keeps every note as a note on the earlier version: its mark is hidden, and you can still send it.
      </p>
      <div class="panel-actions">
        <button type="button" class="act-btn primary" onclick={stayOnVersion} data-stay>Stay on this version</button>
        <span class="flex-1"></span>
        <button type="button" class="act-btn" onclick={updateAnyway} data-update-anyway>Update anyway</button>
      </div>
    </section>
  {:else if selection}
    {@const el = selection.element}
    {@const tree = selection.tree}
    {@const depth = tree.ancestors.length}
    <section class="panel select-sheet" aria-label="Selected element">
      <div class="panel-head">
        <p class="m-0 min-w-0 flex-1 truncate font-mono text-xs font-semibold">{nodeName(tree.self)}</p>
        <span class="shrink-0 text-xxs text-muted-foreground">{Math.round(el.rect.width)}×{Math.round(el.rect.height)}</span>
        <button type="button" class="icon-btn" aria-label="Cancel selection" onclick={clearSelection}><Icon name="x" /></button>
      </div>
      {#if selection.state === "gone"}
        <p class="m-0 text-xs text-warning" role="status">
          This element was removed from the page. Tap another element.
        </p>
      {:else if selection.state === "hidden"}
        <p class="m-0 text-xs text-warning" role="status">
          This element has no box of its own on the page (it may be hidden or use display: contents). Pick a
          parent or child from the tree.
        </p>
      {:else}
        <p class="sel-preview">
          {#if el.label}<span class="block truncate text-muted-foreground">{el.role ? `${el.role} · ` : ""}{el.label}</span>{/if}
          <span class="sel-text">{el.text.trim() ? el.text.trim().slice(0, 200) : "No text in this element."}</span>
        </p>
        {#if targetDrawsPixels({ kind: "element", element: el })}
          <p class="m-0 text-xxs text-muted-foreground">
            This part is drawn as pixels the agent cannot see. Attach a screenshot if the picture matters.
          </p>
        {/if}
      {/if}
      <ol class="tree" class:more={treeMore} aria-label="Element tree" bind:this={treeList} onscroll={updateTreeMore}>
        {#each tree.ancestors as node, i (i)}
          <li>
            <button type="button" class="tree-row" style="--depth: {Math.min(i, 6)}" onclick={() => selectNode(node)}>
              <span class="tree-name">{nodeName(node)}</span>
              {#if node.text}<span class="tree-text">{node.text}</span>{/if}
            </button>
          </li>
        {/each}
        <li>
          <span class="tree-row current" style="--depth: {Math.min(depth, 6)}" aria-current="true">
            <span class="tree-name">{nodeName(tree.self)}</span>
            {#if tree.self.text}<span class="tree-text">{tree.self.text}</span>{/if}
          </span>
        </li>
        {#each tree.children as node, i (i)}
          <li>
            <button type="button" class="tree-row" style="--depth: {Math.min(depth + 1, 7)}" onclick={() => selectNode(node)}>
              <span class="tree-name">{nodeName(node)}</span>
              {#if node.text}<span class="tree-text">{node.text}</span>{/if}
            </button>
          </li>
        {/each}
        {#if tree.childCount > tree.children.length}
          <li class="tree-more" style="--depth: {Math.min(depth + 1, 7)}">
            +{tree.childCount - tree.children.length} more children: tap one on the page
          </li>
        {/if}
      </ol>
      <div class="panel-actions">
        <button
          type="button"
          class="act-btn"
          onclick={() => tree.ancestors.length && selectNode(tree.ancestors[tree.ancestors.length - 1])}
          disabled={tree.ancestors.length === 0 || sending}
        >
          <Icon name="chevron-up" /><span>Select parent</span>
        </button>
        <span class="flex-1"></span>
        <button type="button" class="act-btn primary" onclick={commentOnSelection} disabled={selection.state !== "ok" || sending}>
          <Icon name="message-square" /><span>Comment on this</span>
        </button>
      </div>
    </section>
  {:else if panel === "list"}
    <section class="panel" aria-label="Notes">
      {#if drafts.length === 0}
        <p class="m-0 text-xs text-muted-foreground">
          {isImage
            ? "No notes yet. Use Tap to pin a spot or Draw to mark an area."
            : "No notes yet. Select text in Browse, use Tap to pick an element, or Draw to mark an area."}
        </p>
      {:else}
        <ol class="note-list">
          {#each drafts as draft, index (draft.id)}
            <li class="note-row">
              <button type="button" class="note-main" onclick={() => openEditor(draft, false)} disabled={sending}>
                <span class="note-n">{index + 1}</span>
                <span class="min-w-0 flex-1">
                  <span class="block truncate text-xs">
                    <span class="intent intent-{draft.intent}">{intentLabel(draft.intent)}</span>
                    {#if draft.target.kind === "text"}
                      <mark class="quote-mark">{targetSummary(draft.target, surface)}</mark>
                    {:else}
                      {targetSummary(draft.target, surface)}
                    {/if}
                  </span>
                  <span class="block truncate text-xxs text-muted-foreground">
                    {#if !stale && unresolved.includes(index + 1)}<span class="text-warning">Not found on the page now · </span>{/if}
                    {draft.comment.trim() || (draft.intent === "keep" ? "Keep as is" : "")}
                    {#if draft.voice.length}· {draft.voice.length} voice{/if}
                    {#if draft.images.length}· {draft.images.length} image{/if}
                  </span>
                </span>
              </button>
              <button
                type="button"
                class="icon-btn"
                aria-label="Delete note {index + 1}"
                onclick={() => deleteDraft(draft.id)}
                disabled={sending}
              >
                <Icon name="trash" />
              </button>
            </li>
          {/each}
        </ol>
      {/if}
      {#if stale !== "reopened"}
        <button type="button" class="act-btn self-start" onclick={addPageNote} disabled={!loaded || sending}>
          <Icon name="plus" /><span>Note on the whole {surface}</span>
        </button>
      {/if}
    </section>
  {/if}

  {#if drafts.length > 0 || sendState !== "idle"}
    <div class="send-bar">
      <p class="m-0 min-w-0 flex-1 text-xxs" class:text-destructive={sendState === "error"} class:text-muted-foreground={sendState !== "error"}>
        {#if sendMessage}
          {sendMessage}
        {:else if sending}
          Sending… Notes are locked until it finishes.
        {:else}
          {#if pendingStrokes > 0}
            Tap Comment to turn your drawing into a note, or Clear it, before sending.
          {:else}
            <span data-storage={storage}>
              {drafts.length} {drafts.length === 1 ? "note" : "notes"}, {storage === "saved"
                ? "saved on this device"
                : storage === "saving"
                  ? "saving on this device…"
                  : "not saved on this device (storage failed), kept until you close the Mini App"}.
            </span>
            Nothing is sent until you tap {stale ? "Send anyway" : "Send"}.
          {/if}
        {/if}
      </p>
      {#if drafts.length > 0 && !stale}
        <button
          type="button"
          class="act-btn primary"
          onclick={() => send()}
          disabled={sending || panel === "editor" || pendingStrokes > 0 || !draftSha || updating}
        >
          <Icon name="send" />
          <span>{sendState === "error" ? "Retry" : "Send"}</span>
        </button>
      {/if}
    </div>
  {/if}
</div>

<style>
  .annot-root {
    display: flex;
    flex: 1;
    flex-direction: column;
    min-height: 24rem;
  }

  .annot-toolbar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.375rem 0.5rem;
    border-bottom: 1px solid var(--color-border);
  }

  .mode-group {
    display: inline-flex;
    flex: 1;
    min-width: 0;
    gap: 0.25rem;
  }

  .mode-btn,
  .notes-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    border-radius: 6px;
    border: 1px solid transparent;
    padding: 0.3rem 0.55rem;
    font-size: 0.75rem;
    color: var(--color-muted-foreground);
  }

  .mode-btn[aria-pressed="true"],
  .notes-btn[aria-pressed="true"] {
    border-color: var(--color-border);
    background: var(--color-accent);
    color: var(--color-foreground);
  }

  .mode-btn:disabled,
  .notes-btn:disabled {
    opacity: 0.45;
  }

  .count {
    min-width: 1.1rem;
    border-radius: 999px;
    background: var(--color-brand);
    padding: 0 0.3rem;
    font-size: var(--text-xxs);
    line-height: 1.1rem;
    color: white;
    text-align: center;
  }

  .annot-stage {
    position: relative;
    min-height: 10rem;
    flex: 1;
    overflow: hidden;
    background: white;
  }

  .annot-frame {
    display: block;
    width: 100%;
    height: 100%;
    border: 0;
    background: white;
  }

  .annot-overlay {
    position: absolute;
    inset: 0;
    touch-action: none;
    cursor: crosshair;
  }

  .copy-note {
    margin: 0;
    border-bottom: 1px solid var(--color-border);
    padding: 0.3rem 0.75rem;
    font-size: var(--text-xxs);
    color: var(--color-muted-foreground);
  }

  .banner {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    border-bottom: 1px solid var(--color-border);
    padding: 0.5rem 0.75rem;
    font-size: 0.75rem;
    color: var(--color-muted-foreground);
  }

  .banner.info {
    border-bottom-color: color-mix(in srgb, var(--color-link) 45%, transparent);
  }

  .checker-frame {
    position: absolute;
    inset: 0;
    opacity: 0;
    pointer-events: none;
  }

  .banner.warn {
    border-bottom-color: var(--color-warning-border);
    color: var(--color-foreground);
  }

  .annot-canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    touch-action: none;
    cursor: crosshair;
  }

  .stage-bar {
    position: absolute;
    left: 0.5rem;
    right: 0.5rem;
    bottom: 0.5rem;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.375rem;
  }

  .stage-bar.passive {
    pointer-events: none;
  }

  .stage-hint {
    margin-right: auto;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.66);
    padding: 0.3rem 0.55rem;
    font-size: var(--text-xxs);
    color: white;
  }

  .stage-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.72);
    padding: 0.4rem 0.65rem;
    font-size: 0.75rem;
    color: white;
  }

  .stage-btn.primary {
    background: var(--color-brand);
  }

  .text-dock {
    position: absolute;
    left: 0.5rem;
    right: 0.5rem;
    bottom: 0.5rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    border-radius: 10px;
    background: rgb(0 0 0 / 0.8);
    padding: 0.45rem 0.45rem 0.45rem 0.7rem;
    color: white;
    box-shadow: 0 6px 18px rgb(0 0 0 / 0.25);
  }

  .text-dock small {
    display: block;
    font-size: var(--text-xxs);
    opacity: 0.7;
  }

  .dock-quote {
    display: -webkit-box;
    overflow: hidden;
    font-size: 0.75rem;
    line-height: 1.3;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
  }

  .quote-block {
    margin: 0;
    border-left: 3px solid var(--color-brand);
    padding: 0.25rem 0.6rem;
    background: color-mix(in srgb, var(--color-brand) 8%, transparent);
    font-size: 0.75rem;
    line-height: 1.4;
    overflow-wrap: anywhere;
  }

  .quote-mark {
    border-radius: 3px;
    background: color-mix(in srgb, var(--color-brand) 16%, transparent);
    padding: 0 0.15rem;
    color: inherit;
  }

  .panel {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    max-height: 55%;
    overflow-y: auto;
    border-top: 1px solid var(--color-border);
    padding: 0.625rem 0.75rem;
  }

  .panel-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .sketch {
    display: block;
    max-height: 7rem;
    width: auto;
    max-width: 100%;
    align-self: flex-start;
    border-radius: 6px;
    border: 1px solid var(--color-border);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 0.375rem;
  }

  .chip {
    border-radius: 999px;
    border: 1px solid var(--color-border);
    padding: 0.2rem 0.7rem;
    font-size: 0.75rem;
    color: var(--color-muted-foreground);
  }

  .chip[aria-checked="true"] {
    border-color: var(--color-foreground);
    background: var(--color-primary);
    color: var(--color-primary-foreground);
  }

  .comment {
    width: 100%;
    resize: vertical;
    border-radius: 8px;
    border: 1px solid var(--color-input-border);
    background: var(--color-input-background);
    padding: 0.5rem 0.625rem;
    font-size: 0.875rem;
    color: var(--color-foreground);
  }

  .media {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .media-row {
    display: flex;
    align-items: center;
    gap: 0.375rem;
  }

  .media-row audio {
    height: 2.25rem;
  }

  .thumbs {
    display: flex;
    flex-wrap: wrap;
    gap: 0.375rem;
  }

  .thumb {
    position: relative;
    width: 4rem;
    height: 4rem;
    overflow: hidden;
    border-radius: 6px;
    border: 1px solid var(--color-border);
  }

  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .thumb-remove {
    position: absolute;
    top: 0.125rem;
    right: 0.125rem;
    border-radius: 999px;
    background: rgb(0 0 0 / 0.6);
    padding: 0.125rem;
    color: white;
  }

  .panel-actions {
    display: flex;
    align-items: center;
    gap: 0.375rem;
  }

  .act-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.375rem;
    border-radius: 6px;
    border: 1px solid var(--color-border);
    background: var(--color-card);
    padding: 0.375rem 0.625rem;
    font-size: 0.75rem;
    color: var(--color-foreground);
  }

  .act-btn.primary {
    border-color: var(--color-primary);
    background: var(--color-primary);
    color: var(--color-primary-foreground);
  }

  .act-btn.recording {
    border-color: var(--color-destructive);
    color: var(--color-destructive);
  }

  .act-btn:disabled {
    opacity: 0.5;
  }

  .icon-btn {
    border-radius: 6px;
    padding: 0.375rem;
    color: var(--color-muted-foreground);
  }

  .icon-btn.danger {
    color: var(--color-destructive);
  }

  .act-btn:hover:not(:disabled),
  .icon-btn:hover {
    background: var(--color-accent);
  }

  .act-btn.primary:hover:not(:disabled) {
    background: var(--color-primary-hover);
  }

  .select-sheet {
    max-height: 42%;
    gap: 0.375rem;
    padding-top: 0.5rem;
    padding-bottom: 0.5rem;
  }

  .sel-preview {
    margin: 0;
    border-left: 3px solid #286bbd;
    padding: 0.125rem 0 0.125rem 0.5rem;
    font-size: 0.75rem;
  }

  .sel-text {
    display: -webkit-box;
    overflow: hidden;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    word-break: break-word;
  }

  .tree {
    margin: 0;
    /* Exactly two whole rows. Every row is the same height, so every scroll
       position, the last one included, starts on a row edge: no row is ever
       cut in half at the top. The fade says there is more below. */
    box-sizing: content-box;
    max-height: calc(2.75rem * 2);
    flex-shrink: 0;
    scroll-snap-type: y mandatory;
    overflow-y: auto;
    padding: 0;
    list-style: none;
    border-top: 1px solid var(--color-border);
    border-bottom: 1px solid var(--color-border);
  }

  .tree li {
    scroll-snap-align: start;
  }

  .tree.more {
    mask-image: linear-gradient(to bottom, black calc(100% - 0.75rem), transparent);
  }

  .tree-row {
    display: flex;
    width: 100%;
    height: 2.75rem;
    align-items: center;
    gap: 0.5rem;
    padding: 0.25rem 0.25rem 0.25rem calc(0.25rem + var(--depth) * 0.75rem);
    text-align: left;
    color: var(--color-muted-foreground);
  }

  button.tree-row:hover {
    background: var(--color-accent);
  }

  .tree-row.current {
    border-radius: 4px;
    background: color-mix(in srgb, #286bbd 14%, transparent);
    color: var(--color-foreground);
  }

  .tree-name {
    flex: none;
    max-width: 55%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }

  .tree-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: var(--text-xxs);
  }

  .tree-more {
    display: flex;
    height: 2.75rem;
    align-items: center;
    padding: 0.25rem 0.25rem 0.25rem calc(0.25rem + var(--depth) * 0.75rem);
    font-size: var(--text-xxs);
    color: var(--color-muted-foreground);
  }

  .note-list {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .note-row {
    display: flex;
    align-items: center;
    gap: 0.25rem;
  }

  .note-row + .note-row {
    border-top: 1px solid var(--color-border);
  }

  .note-main {
    display: flex;
    min-width: 0;
    flex: 1;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0;
    text-align: left;
    color: var(--color-foreground);
  }

  .note-n {
    display: inline-flex;
    flex: none;
    width: 1.25rem;
    height: 1.25rem;
    align-items: center;
    justify-content: center;
    border-radius: 999px;
    background: var(--color-brand);
    font-size: var(--text-xxs);
    font-weight: 600;
    color: white;
  }

  .intent {
    margin-right: 0.25rem;
    font-weight: 600;
  }

  .intent-keep {
    color: var(--color-success);
  }

  .intent-question {
    color: var(--color-link);
  }

  .send-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    border-top: 1px solid var(--color-border);
    padding: 0.5rem 0.75rem;
  }
</style>
