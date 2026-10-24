<script lang="ts">
  import { formatSecs, runAges } from "$lib/run";
  import type { RunStatus } from "$lib/types";

  interface Props {
    run: RunStatus;
  }

  let { run }: Props = $props();

  let ages = $derived(runAges(run));
</script>

<!-- What the running turn does now. Ages are shown as observed; nothing here
     calls a quiet run stuck. -->
<div class="shrink-0 border-b border-border px-3 py-1.5" aria-live="polite">
  <div class="flex items-baseline gap-2">
    <span class="size-1.5 shrink-0 animate-pulse rounded-full bg-warning" aria-hidden="true"></span>
    <span class="min-w-0 flex-1 truncate font-mono text-xxs text-foreground">{run.label}</span>
    {#if run.turn_elapsed_secs !== null}
      <span class="shrink-0 font-mono text-xxs text-muted-foreground"
        >{formatSecs(run.turn_elapsed_secs)}</span
      >
    {/if}
  </div>
  {#if ages}
    <p class="m-0 truncate pl-3.5 font-mono text-xxs text-muted-foreground">{ages}</p>
  {/if}
</div>
