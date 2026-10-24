<script lang="ts">
  import { untrack } from "svelte";
  import { api, formatBytes, ApiError } from "$lib/api";
  import { findLinkedArtifact } from "$lib/artifactLink";
  import {
    ARTIFACT_SORT_LABELS,
    clockLabel,
    fullDateLabel,
    groupArtifacts,
    modifiedTime,
    shortDateLabel,
    sortArtifacts,
    type ArtifactSort,
  } from "$lib/artifactList";
  import { router } from "$lib/router.svelte";
  import type { ArtifactItem } from "$lib/types";
  import ArtifactPreview from "../components/ArtifactPreview.svelte";
  import Icon from "../components/Icon.svelte";

  interface Props {
    threadId: string;
    ongoto: (sequence: number) => void;
  }

  let { threadId, ongoto }: Props = $props();

  let items = $state<ArtifactItem[]>([]);
  let error = $state("");
  let loading = $state(true);
  let preview = $state<ArtifactItem | null>(null);
  /** Why a file's `Open artifact` link could not open it, if it could not. */
  let linkNotice = $state("");
  /** Thread the current `items` belong to; also the in-flight staleness key. */
  let loadedFor = $state<string | null>(null);
  const SORTS: ArtifactSort[] = ["newest", "oldest", "name"];
  let sort = $state<ArtifactSort>("newest");
  let sortOpen = $state(false);
  /** When the list was loaded: `Today` / `Yesterday` are measured from it. */
  let loadedAt = $state(Date.now());
  /** Rows whose time is expanded to the full date. */
  let expanded = $state<Set<string>>(new Set());

  let groups = $derived(groupArtifacts(sortArtifacts(items, sort), sort, loadedAt));

  function pickSort(next: ArtifactSort) {
    sort = next;
    sortOpen = false;
  }

  function toggleTime(path: string) {
    const next = new Set(expanded);
    if (!next.delete(path)) next.add(path);
    expanded = next;
  }

  function timeLabel(item: ArtifactItem, t: number): string {
    if (expanded.has(item.path)) return fullDateLabel(t);
    return sort === "name" ? shortDateLabel(t) : clockLabel(t);
  }

  function sourceLabel(source: ArtifactItem["source"]): string {
    return source === "both" ? "generated + sent" : source;
  }

  async function load() {
    const target = threadId;
    loadedFor = target;
    loading = true;
    error = "";
    try {
      const response = await api.artifacts(target);
      if (loadedFor !== target) return;
      loadedAt = Date.now();
      items = response.artifacts;
      openLinked(target, response.artifacts);
    } catch (e) {
      if (loadedFor === target) error = e instanceof ApiError ? e.message : String(e);
    } finally {
      if (loadedFor === target) loading = false;
    }
  }

  // An `Open artifact` link names one file by its projection key; open it
  // with the same preview (and feedback destination) as a tap on its row.
  function openLinked(target: string, list: ArtifactItem[]) {
    const key = router.takeLinkedArtifact(target);
    if (!key) return;
    const item = findLinkedArtifact(list, key);
    if (item?.exists) {
      preview = item;
      return;
    }
    linkNotice = item
      ? `${item.name} is no longer available.`
      : "The linked file is not in this thread's artifact list. It may no longer be part of this thread, or be older than the newest 500 files the list shows.";
  }

  async function download(item: ArtifactItem) {
    const blob = await api.artifactBlob(threadId, item.path);
    const objectUrl = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = objectUrl;
    anchor.download = item.name;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    setTimeout(() => URL.revokeObjectURL(objectUrl), 30_000);
  }

  // Track the thread id and nothing else. `load` reads `loadedFor` before its
  // first await and then writes it, so calling it tracked makes this effect
  // depend on its own write: it re-runs, resets `loading` to true, fetches
  // again, and never settles — the pane sits on "Loading artifacts…" while
  // hammering the endpoint. `untrack` keeps the dependency set to `threadId`.
  $effect(() => {
    const target = threadId;
    untrack(() => {
      items = [];
      linkNotice = "";
      void target;
      void load();
    });
    // A response that lands after this pane is gone (or moved to another
    // thread) is dropped, so it cannot take a link meant for the next pane.
    return () => {
      loadedFor = null;
    };
  });
</script>

<svelte:window
  onclick={(event) => {
    if (sortOpen && !(event.target as Element | null)?.closest("[data-sort-root]")) sortOpen = false;
  }}
  onkeydown={(event) => {
    if (event.key === "Escape") sortOpen = false;
  }}
/>

<div class="scroll-area h-full px-3 py-2">
  <div class="mx-auto flex max-w-3xl flex-col gap-2 pb-6">
    {#if linkNotice}
      <p class="rounded-md border border-border px-3 py-2 text-xs text-muted-foreground" data-link-notice>
        {linkNotice}
      </p>
    {/if}
    {#if loading}
      <p class="py-8 text-center text-xs text-muted-foreground">Loading artifacts…</p>
    {:else if error}
      <p class="rounded-md border border-destructive px-3 py-2 text-xs text-destructive">{error}</p>
    {:else if items.length === 0}
      <p class="py-8 text-center text-xs text-muted-foreground">
        Nothing here yet. Files this thread generates or sends will appear in this tab.
      </p>
    {:else}
      <div class="flex items-center gap-2">
        <span class="text-xxs text-muted-foreground">{items.length} {items.length === 1 ? "file" : "files"}</span>
        <div class="relative ml-auto" data-sort-root>
          <button
            type="button"
            class="sort-chip"
            aria-haspopup="menu"
            aria-expanded={sortOpen}
            onclick={() => (sortOpen = !sortOpen)}
            data-artifact-sort
          >
            <Icon name="arrow-up-down" size="0.75rem" />
            <span>{ARTIFACT_SORT_LABELS[sort]}</span>
            <Icon name="chevron-down" size="0.75rem" />
          </button>
          {#if sortOpen}
            <div class="sort-menu" role="menu">
              {#each SORTS as option (option)}
                <button
                  type="button"
                  role="menuitemradio"
                  aria-checked={sort === option}
                  onclick={() => pickSort(option)}
                >
                  {ARTIFACT_SORT_LABELS[option]}
                </button>
              {/each}
            </div>
          {/if}
        </div>
      </div>
      {#each groups as group, gi (gi)}
        {#if group.label !== null || sort !== "name"}
          <p class="day" data-artifact-day>
            {group.label ?? "No date"}
            <span>· {group.rows.length} {group.rows.length === 1 ? "file" : "files"}</span>
          </p>
        {/if}
        {#each group.rows as item, ii (ii)}
          {@const t = modifiedTime(item)}
          <div class="rounded-xl border border-border bg-card px-3 py-2.5">
            <div class="flex items-center gap-2">
              <span class="text-muted-foreground"><Icon name="file" /></span>
              <span class="min-w-0 flex-1">
                <span class="block truncate font-mono text-xs">{item.name}</span>
                <span class="block truncate text-xxs text-muted-foreground">
                  {formatBytes(item.size_bytes)} · {sourceLabel(item.source)}
                  {#if t !== null}
                    ·
                    <button
                      type="button"
                      class="time"
                      title={fullDateLabel(t)}
                      onclick={() => toggleTime(item.path)}
                      data-artifact-time
                    >
                      <time datetime={item.modified_at}>{timeLabel(item, t)}</time>
                    </button>
                  {/if}
                </span>
              </span>
              {#if !item.exists}
                <span class="text-xxs text-muted-foreground">unavailable</span>
              {/if}
            </div>
            <div class="mt-2 flex flex-wrap items-center gap-1.5">
              {#if item.exists}
                <button type="button" onclick={() => (preview = item)} class="row-btn"> Open </button>
                <button
                  type="button"
                  onclick={() => download(item)}
                  class="row-btn row-btn-icon"
                  aria-label="Download {item.name}"
                >
                  <Icon name="download" />
                </button>
              {/if}
              {#if item.message_sequences.length > 0}
                <button type="button" onclick={() => ongoto(item.message_sequences[0])} class="row-btn">
                  Go to message
                </button>
              {:else}
                <span class="text-xxs text-muted-foreground">not linked to a message</span>
              {/if}
            </div>
          </div>
        {/each}
      {/each}
    {/if}
  </div>
</div>

{#if preview}
  <ArtifactPreview threadId={threadId} artifact={preview} onclose={() => (preview = null)} />
{/if}

<style>
  .row-btn {
    display: inline-flex;
    align-items: center;
    border-radius: 6px;
    border: 1px solid var(--color-border);
    padding: 0.25rem 0.625rem;
    font-size: 0.75rem;
    color: var(--color-foreground);
  }

  .row-btn-icon {
    padding: 0.25rem 0.375rem;
  }

  .row-btn:hover {
    background: var(--color-accent);
  }

  .day {
    margin: 0.5rem 0 0;
    padding: 0 0.125rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-foreground);
  }

  .day span {
    color: var(--color-muted-foreground);
    font-weight: 400;
    letter-spacing: 0;
    text-transform: none;
  }

  .time {
    color: inherit;
    text-decoration: underline dotted;
    text-underline-offset: 2px;
  }

  .sort-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    border-radius: 999px;
    border: 1px solid var(--color-border);
    padding: 0.2rem 0.55rem;
    font-size: 0.6875rem;
    color: var(--color-foreground);
  }

  .sort-chip:hover {
    background: var(--color-accent);
  }

  .sort-menu {
    position: absolute;
    right: 0;
    top: calc(100% + 4px);
    z-index: 10;
    display: flex;
    min-width: 9rem;
    flex-direction: column;
    padding: 0.25rem;
    border-radius: 8px;
    border: 1px solid var(--color-border);
    background: var(--color-surface);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.18);
  }

  .sort-menu button {
    padding: 0.35rem 0.5rem;
    border-radius: 6px;
    text-align: left;
    font-size: 0.75rem;
    color: var(--color-muted-foreground);
  }

  .sort-menu button[aria-checked="true"] {
    background: var(--color-accent);
    color: var(--color-foreground);
    font-weight: 500;
  }
</style>
