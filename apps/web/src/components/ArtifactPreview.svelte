<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api, ApiError } from "$lib/api";
  import { draftKey, loadDraftRecord } from "$lib/draftStore";
  import { confirmAction, telegramUserId } from "$lib/telegram";
  import type { ArtifactAttachment, ArtifactImageCopy } from "$lib/types";
  import ArtifactAnnotator from "./ArtifactAnnotator.svelte";
  import Icon from "./Icon.svelte";

  interface Props {
    threadId: string;
    artifact: ArtifactAttachment;
    onclose: () => void;
  }

  let { threadId, artifact, onclose }: Props = $props();

  let url = $state<string | null>(null);
  /** HTML bytes as served, for the annotator (it injects its helper into a copy). */
  let htmlBlob = $state<Blob | null>(null);
  /** An image's canonical copy from the server: what is shown, marked, and
   *  sent to the agent. The app never re-encodes or re-orients it. */
  let imageCopy = $state<ArtifactImageCopy | null>(null);
  /** Why an image is shown as a preview only (the server's reason). */
  let passiveReason = $state<string | null>(null);
  let error = $state("");
  let loading = $state(true);
  /** The annotator has an open note, a recording, or a stroke closing would lose. */
  let unsaved = $state(false);

  /** Saved notes survive closing; only unsaved editor work asks first. */
  async function requestClose() {
    if (unsaved && !(await confirmAction("Discard the note you are editing?"))) return;
    onclose();
  }

  /** Fetches the bytes once and keeps the blob URL for the open preview. */
  async function fetchBytes(): Promise<string | null> {
    const existing = untrack(() => url);
    if (existing) return existing;
    try {
      const blob = await api.artifactBlob(threadId, artifact.path);
      const objectUrl = URL.createObjectURL(blob);
      url = objectUrl;
      return objectUrl;
    } catch (e) {
      error = e instanceof ApiError ? e.message : String(e);
      return null;
    } finally {
      loading = false;
    }
  }

  async function save() {
    const objectUrl = await fetchBytes();
    if (!objectUrl) return;
    const anchor = document.createElement("a");
    anchor.href = objectUrl;
    anchor.download = artifact.name;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  }

  /** The canonical copy when the server can make one; otherwise the original,
   *  shown as a passive preview with the server's reason. */
  async function loadImage(target: string, path: string): Promise<{ copy: ArtifactImageCopy | null; reason: string | null; blob: Blob | null }> {
    try {
      return { copy: await api.artifactImage(target, path), reason: null, blob: null };
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 422) throw e;
      // Notes made while the image could be annotated stay on the device
      // untouched; say so instead of hiding them.
      const record = await loadDraftRecord(draftKey(telegramUserId(), target, path)).catch(() => null);
      const kept = record?.drafts.length ?? 0;
      const notes =
        kept > 0
          ? ` Your ${kept === 1 ? "unsent note on it is" : `${kept} unsent notes on it are`} kept on this device.`
          : "";
      return {
        copy: null,
        reason: `${e.message} It is shown as a preview only.${notes}`,
        blob: await api.artifactBlob(target, path),
      };
    }
  }

  // Tracks the artifact identity only; the fetch writes `url`/`loading`/`error`
  // and must not feed back into this effect's dependency set.
  $effect(() => {
    const target = threadId;
    const path = artifact.path;
    const kind = artifact.kind;
    const exists = artifact.exists;

    return untrack(() => {
      const previous = url;
      url = null;
      htmlBlob = null;
      imageCopy = null;
      passiveReason = null;
      if (previous) URL.revokeObjectURL(previous);
      loading = true;
      error = "";
      // Images, audio and HTML need bytes to render. Other kinds fetch lazily
      // through the Download button instead.
      if (kind === "other" || !exists) {
        loading = false;
        return;
      }
      let cancelled = false;
      const load =
        kind === "image"
          ? loadImage(target, path)
          : api.artifactBlob(target, path).then((blob) => ({ copy: null, reason: null, blob }));
      void load
        .then(({ copy, reason, blob }) => {
          if (cancelled) return;
          passiveReason = reason;
          if (copy) imageCopy = copy;
          else if (blob) {
            if (kind === "html") htmlBlob = blob;
            url = URL.createObjectURL(blob);
          }
        })
        .catch((e: unknown) => {
          if (!cancelled) error = e instanceof ApiError ? e.message : String(e);
        })
        .finally(() => {
          if (!cancelled) loading = false;
        });
      return () => {
        cancelled = true;
      };
    });
  });

  onDestroy(() => {
    if (url) URL.revokeObjectURL(url);
  });

  function backdropKey(event: KeyboardEvent) {
    // Escape in a text field cancels the field, not the whole preview.
    const target = event.target as HTMLElement | null;
    if (target?.closest("input, textarea, [contenteditable]")) return;
    if (event.key === "Escape") void requestClose();
  }
</script>

<svelte:window onkeydown={backdropKey} />

<div
  class="preview-backdrop"
  role="presentation"
  onclick={(e) => {
    if (e.target === e.currentTarget) void requestClose();
  }}
>
  <div class="preview-sheet" role="dialog" aria-label={artifact.name}>
    <header class="flex shrink-0 items-center gap-2 border-b border-border px-3 py-2">
      <div class="min-w-0 flex-1">
        <p class="m-0 truncate text-sm font-semibold">{artifact.name}</p>
        <p class="m-0 truncate font-mono text-xxs text-muted-foreground">{artifact.mime}</p>
      </div>
      {#if artifact.kind === "other"}
        <button type="button" onclick={save} class="preview-btn">
          <Icon name="download" />
          <span>Download</span>
        </button>
      {/if}
      <button type="button" onclick={requestClose} aria-label="Close preview" class="preview-icon-btn">
        <Icon name="x" />
      </button>
    </header>

    <div class="preview-body">
      {#if !artifact.exists}
        <p class="py-8 text-center text-xs text-muted-foreground">
          This file is no longer on the bot host, so there is nothing to preview.
        </p>
      {:else if loading}
        <p class="py-8 text-center text-xs text-muted-foreground">Loading preview…</p>
      {:else if error}
        <p class="mx-3 rounded-md border border-destructive px-3 py-2 text-xs text-destructive">
          {error}
        </p>
      {:else if artifact.kind === "html" && htmlBlob}
        <ArtifactAnnotator {threadId} {artifact} blob={htmlBlob} bind:unsaved />
      {:else if artifact.kind === "image" && imageCopy}
        <ArtifactAnnotator {threadId} {artifact} blob={imageCopy.blob} image={imageCopy} bind:unsaved />
      {:else if artifact.kind === "image" && url}
        {#if passiveReason}<p class="passive-note">{passiveReason}</p>{/if}
        <img src={url} alt={artifact.name} class="preview-media" />
      {:else if artifact.kind === "audio" && url}
        <div class="p-4">
          <audio controls src={url} class="w-full"></audio>
        </div>
      {:else}
        <div class="flex flex-col items-center gap-2 py-8">
          <p class="m-0 text-xs text-muted-foreground">No inline preview for this file type.</p>
          <button type="button" onclick={save} class="preview-btn">
            <Icon name="download" />
            <span>Download</span>
          </button>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .preview-backdrop {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    background: rgb(0 0 0 / 0.45);
  }

  .preview-sheet {
    display: flex;
    flex-direction: column;
    width: 100%;
    max-width: 48rem;
    max-height: calc(100dvh - var(--tg-safe-top) - 1rem);
    margin-bottom: var(--tg-safe-bottom);
    overflow: hidden;
    background: var(--color-surface);
    border-radius: 12px 12px 0 0;
    box-shadow: 0 -8px 32px rgb(0 0 0 / 0.3);
  }

  .preview-body {
    min-height: 0;
    flex: 1;
    overflow: auto;
  }

  .preview-media {
    display: block;
    width: 100%;
    height: auto;
    max-height: calc(100dvh - 12rem);
    object-fit: contain;
    background: var(--color-accent);
  }

  .passive-note {
    margin: 0;
    border-bottom: 1px solid var(--color-border);
    padding: 0.5rem 0.75rem;
    font-size: 0.75rem;
    color: var(--color-muted-foreground);
  }

  .preview-btn {
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

  .preview-icon-btn {
    border-radius: 6px;
    padding: 0.375rem;
    color: var(--color-muted-foreground);
  }

  .preview-icon-btn:hover,
  .preview-btn:hover {
    background: var(--color-accent);
  }
</style>
