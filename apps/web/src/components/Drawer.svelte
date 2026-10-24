<script lang="ts">
  import { api } from "$lib/api";
  import { currentThread } from "$lib/currentThread.svelte";
  import { jobFailed, peakQuota, servicesLabel } from "$lib/monitorSummary";
  import { router, type MonitorSection, type ThreadTab } from "$lib/router.svelte";
  import { formatSecs } from "$lib/run";
  import { selectionChanged } from "$lib/telegram";
  import type { MonitorResponse, ThreadListItem, WorkerItem } from "$lib/types";
  import Icon, { type IconName } from "./Icon.svelte";

  interface Props {
    open: boolean;
    onclose: () => void;
  }

  let { open, onclose }: Props = $props();

  /** Recent threads shown under the card. */
  const RECENT = 3;
  /** Workers listed inside the card. */
  const CARD_WORKERS = 3;

  let route = $derived(router.current);
  /** The open thread's card, only while its view is on screen. */
  let card = $derived(
    route.view === "thread" && currentThread.value?.routeId === route.id ? currentThread.value : null,
  );

  let threads = $state<ThreadListItem[] | null>(null);
  let monitor = $state<MonitorResponse | null>(null);
  /** Workers fetched by the menu when the view has not loaded them yet. */
  let fetchedWorkers = $state<{ id: string; workers: WorkerItem[] } | null>(null);

  // One read of each source per open; nothing polls while the menu is open,
  // except the card's live line, which follows the thread view's own poll.
  $effect(() => {
    if (!open) return;
    void api
      .threads()
      .then((r) => (threads = r.threads))
      .catch(() => {});
    void api
      .monitor()
      .then((r) => (monitor = r))
      .catch(() => {});
  });

  // The card's workers, read once per open when the thread view has not
  // loaded them (it does only on the Workers and Agent tabs).
  let needWorkersFor = $derived(
    open && card && card.workers === null && card.workerCount > 0 ? card.id : null,
  );
  $effect(() => {
    const id = needWorkersFor;
    if (!id) return;
    void api
      .workers(id)
      .then((r) => (fetchedWorkers = { id, workers: r.workers }))
      .catch(() => {});
  });

  let workers = $derived(
    card ? (card.workers ?? (fetchedWorkers?.id === card.id ? fetchedWorkers.workers : null)) : null,
  );

  const WORKER_ORDER: Record<string, number> = { failed: 0, running: 1, queued: 2 };
  function workerRank(w: WorkerItem): number {
    if (!w.live) return 4;
    return WORKER_ORDER[w.status] ?? 3;
  }
  let topWorkers = $derived(
    workers ? [...workers].sort((a, b) => workerRank(a) - workerRank(b)).slice(0, CARD_WORKERS) : [],
  );

  function workerNote(w: WorkerItem): string {
    if (!w.live) return "status unavailable";
    if (w.status === "running") {
      return w.run?.turn_elapsed_secs != null ? formatSecs(w.run.turn_elapsed_secs) : "running";
    }
    if (w.status === "completed") return w.age ? `done ${w.age}` : "done";
    return w.status;
  }

  let changed = $derived(
    card?.git
      ? card.git.files.staged.length + card.git.files.unstaged.length + card.git.files.untracked.length
      : null,
  );

  let recent = $derived(
    (threads ?? []).filter((t) => t.id !== card?.id && t.id !== route.id).slice(0, RECENT),
  );

  let failedJobs = $derived(monitor ? monitor.jobs.filter(jobFailed).length : null);

  interface Row {
    section: MonitorSection;
    icon: IconName;
    label: string;
    /** Shown on the right; null when the menu has no figure for it. */
    value: string | null;
    bad?: boolean;
  }
  // All of zdx as one list. Every value comes from the one `/api/monitor`
  // read; Workspaces and Config have no figure there, so they show none
  // rather than costing another request.
  let rows = $derived<Row[]>([
    { section: "overview", icon: "layout-grid", label: "Overview", value: null },
    {
      section: "agents",
      icon: "activity",
      label: "Active agents",
      value: monitor ? String(monitor.active_agents.length) : "…",
    },
    { section: "services", icon: "server", label: "Services", value: monitor ? servicesLabel(monitor) : "…" },
    { section: "usage", icon: "gauge", label: "Quota", value: monitor ? peakQuota(monitor) : "…" },
    failedJobs
      ? { section: "jobs", icon: "clock", label: "Jobs", value: `${failedJobs} failed`, bad: true }
      : { section: "jobs", icon: "clock", label: "Jobs", value: monitor ? String(monitor.jobs.length) : "…" },
    { section: "workspaces", icon: "folder", label: "Workspaces", value: null },
    { section: "config", icon: "sliders", label: "Config", value: null },
  ]);

  function go(action: () => void) {
    selectionChanged();
    action();
    onclose();
  }

  const goTab = (tab: ThreadTab) => go(() => router.setTab(tab));
  const goThread = (id: string) => go(() => router.openThread(id));
  const goMonitor = (section: MonitorSection) => go(() => router.openMonitor(section));
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="fixed inset-0 z-30 flex" role="dialog" aria-modal="true" aria-label="Menu">
    <div class="drawer flex w-72 max-w-[85vw] flex-col bg-surface" data-sidebar="sidebar">
      <div
        class="flex items-center gap-2 border-b border-border px-3 py-2.5"
        style="padding-top: calc(var(--tg-safe-top) + 0.625rem)"
      >
        <span class="font-mono text-sm font-semibold tracking-tight">zdx</span>
        <button
          type="button"
          onclick={onclose}
          aria-label="Close menu"
          class="ml-auto rounded-sm p-1 text-muted-foreground hover:bg-accent"
        >
          <Icon name="x" size="1rem" />
        </button>
      </div>

      <div class="scroll-area flex-1 px-2 py-3" data-sidebar="content">
        {#if card}
          <div data-sidebar="group">
            <p class="grp">This thread</p>
            <div class="tcard" data-thread-card>
              <div class="row">
                {#if card.run}
                  <span class="dot dot-live" aria-hidden="true"></span>
                  <span class="text-warning">
                    running{card.run.turn_elapsed_secs !== null ? ` ${formatSecs(card.run.turn_elapsed_secs)}` : ""}
                  </span>
                {:else}
                  <span class="dot" aria-hidden="true"></span>
                  <span>idle</span>
                {/if}
                {#if card.model || card.contextPct !== null}
                  <span class="ml-auto min-w-0 truncate">
                    {[card.model, card.contextPct !== null ? `${card.contextPct.toFixed(0)}%` : null]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                {/if}
              </div>
              <button type="button" class="ttl" onclick={() => goTab("transcript")}>{card.title}</button>
              {#if card.git}
                <button type="button" class="row link-row" onclick={() => goTab("changes")}>
                  <Icon name="git-branch" size="0.75rem" />
                  <span class="truncate">{card.git.repository.branch}</span>
                  {#if changed}
                    <span class="text-link">· {changed} changed</span>
                  {/if}
                </button>
              {/if}
              {#if card.run}
                <div class="live" data-live-line>
                  <span class="spin" aria-hidden="true"></span>
                  <span class="min-w-0 flex-1 truncate">{card.run.label}</span>
                  <small>{card.run.state}</small>
                </div>
              {/if}
              {#if card.workerCount > 0}
                <ul class="wk" data-card-workers>
                  {#if workers === null}
                    <li class="more">{card.workerCount} {card.workerCount === 1 ? "worker" : "workers"} · loading…</li>
                  {:else}
                    {#each topWorkers as worker, wi (wi)}
                      <li>
                        <button type="button" onclick={() => goThread(worker.thread_id)}>
                          <span
                            class="dot"
                            class:dot-bad={worker.live && worker.status === "failed"}
                            class:dot-live={worker.live && (worker.status === "running" || worker.status === "queued")}
                            class:dot-unknown={!worker.live}
                            aria-hidden="true"
                          ></span>
                          <span class="min-w-0 flex-1 truncate">{worker.title}</span>
                          <small class:text-destructive={worker.live && worker.status === "failed"}>
                            {workerNote(worker)}
                          </small>
                        </button>
                      </li>
                    {/each}
                    <li>
                      <button type="button" class="more" onclick={() => goTab("workers")}>
                        {workers.length > topWorkers.length
                          ? `+${workers.length - topWorkers.length} more · all workers`
                          : "all workers"}
                        <Icon name="chevron-right" size="0.7rem" />
                      </button>
                    </li>
                  {/if}
                </ul>
              {/if}
            </div>
          </div>
        {/if}

        <div data-sidebar="group" class:mt-4={!!card}>
          <p class="grp">
            Recent
            <button type="button" class="grp-link" onclick={() => go(() => router.openThreadList())}>
              all <Icon name="chevron-right" size="0.65rem" />
            </button>
          </p>
          <ul data-sidebar="menu">
            {#if threads === null}
              <li class="px-2 py-1 text-xxs text-muted-foreground">Loading…</li>
            {:else if recent.length === 0}
              <li class="px-2 py-1 text-xxs text-muted-foreground">No other threads yet.</li>
            {/if}
            {#each recent as thread, ti (ti)}
              <li data-sidebar="menu-item">
                <button type="button" data-sidebar="menu-button" class="item" onclick={() => goThread(thread.id)}>
                  <span class="dot" class:dot-live={!!thread.run} aria-hidden="true"></span>
                  <span class="min-w-0 flex-1 truncate">{thread.title}</span>
                  {#if thread.age}<span class="n">{thread.age}</span>{/if}
                </button>
              </li>
            {/each}
          </ul>
        </div>

        <div data-sidebar="group" class="mt-4">
          <p class="grp">All of zdx</p>
          <ul class="rows" data-monitor-rows>
            {#each rows as row (row.section)}
              <li>
                <button
                  type="button"
                  class="mrow"
                  class:mrow-bad={row.bad}
                  data-active={route.view === "monitor" && route.section === row.section}
                  onclick={() => goMonitor(row.section)}
                >
                  <Icon name={row.icon} size="0.875rem" />
                  <span class="min-w-0 flex-1 truncate">{row.label}</span>
                  {#if row.value !== null}<b>{row.value}</b>{/if}
                  <Icon name="chevron-right" size="0.75rem" />
                </button>
              </li>
            {/each}
          </ul>
        </div>
      </div>

      <div
        class="border-t border-border px-3 py-2 font-mono text-xxs text-muted-foreground"
        data-sidebar="footer"
        style="padding-bottom: calc(var(--tg-safe-bottom) + 0.5rem)"
      >
        <span class="block truncate" dir="rtl">{route.id}</span>
      </div>
    </div>

    <div class="scrim flex-1 bg-black/50" onclick={onclose}></div>
  </div>
{/if}

<style>
  .grp {
    display: flex;
    align-items: center;
    margin: 0 0 0.25rem;
    padding: 0 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-muted-foreground);
  }

  .grp-link {
    display: inline-flex;
    align-items: center;
    gap: 0.1rem;
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
    color: var(--color-link);
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .item {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.5rem;
    border-radius: 6px;
    text-align: left;
    font-size: 0.8125rem;
    color: var(--color-foreground);
  }

  .item:hover {
    background: var(--color-accent);
  }

  .n {
    flex: none;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--color-muted-foreground);
  }

  .dot {
    flex: none;
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--color-muted-foreground) 45%, transparent);
  }

  .dot-live {
    background: var(--color-warning);
  }

  .dot-bad {
    background: var(--color-destructive);
  }

  .dot-unknown {
    background: transparent;
    border: 1px dashed var(--color-muted-foreground);
  }

  .tcard {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    margin: 0 0.125rem;
    padding: 0.6rem 0.65rem;
    border-radius: 10px;
    border: 1px solid var(--color-border);
    background: var(--color-card);
  }

  .row {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 0.35rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--color-muted-foreground);
  }

  .link-row {
    text-align: left;
  }

  .ttl {
    display: block;
    text-align: left;
    font-size: 0.875rem;
    font-weight: 600;
    line-height: 1.25;
    letter-spacing: -0.01em;
    color: var(--color-foreground);
    overflow-wrap: anywhere;
  }

  .live {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.3rem 0.45rem;
    border-radius: 6px;
    background: var(--color-accent);
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--color-foreground);
  }

  .live small {
    flex: none;
    color: var(--color-muted-foreground);
  }

  .spin {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 999px;
    border: 1.5px solid var(--color-warning);
    border-right-color: transparent;
    animation: spin 0.9s linear infinite;
  }

  .wk {
    display: flex;
    flex-direction: column;
    margin-top: 0.1rem;
    border-top: 1px solid var(--color-border);
    padding-top: 0.3rem;
  }

  .wk button {
    display: flex;
    width: 100%;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
    text-align: left;
    font-size: 0.75rem;
    color: var(--color-foreground);
  }

  .wk small {
    flex: none;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--color-muted-foreground);
  }

  .wk .more,
  .wk li.more {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0.2rem 0;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    color: var(--color-link);
  }

  .wk li.more {
    color: var(--color-muted-foreground);
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    padding: 0 0.125rem;
  }

  .mrow {
    display: flex;
    width: 100%;
    min-width: 0;
    align-items: center;
    gap: 0.55rem;
    padding: 0.6rem 0.65rem;
    border-radius: 8px;
    border: 1px solid var(--color-border);
    text-align: left;
    font-size: 0.8125rem;
    color: var(--color-foreground);
  }

  .mrow b {
    flex: none;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    font-weight: 600;
  }

  .mrow :global(svg:last-child) {
    color: var(--color-muted-foreground);
  }

  .mrow-bad b {
    color: var(--color-destructive);
  }

  .mrow:hover,
  .mrow[data-active="true"] {
    background: var(--color-accent);
  }

  .drawer {
    animation: slide-in 180ms cubic-bezier(0.2, 0.8, 0.2, 1);
  }
  .scrim {
    animation: fade-in 180ms ease-out;
  }

  @keyframes slide-in {
    from {
      transform: translateX(-100%);
    }
  }
  @keyframes fade-in {
    from {
      opacity: 0;
    }
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .drawer,
    .scrim,
    .spin {
      animation: none;
    }
  }
</style>
