/**
 * What the open thread view already knows about its thread, for the menu's
 * thread card. `ThreadView` publishes it from its own fetches, so opening the
 * menu costs no request for the card and shows the same live run the view
 * polls. Fields the view has not fetched (workers before their first load,
 * git before the Agent or Changes tab) stay null: the card leaves them out
 * rather than guessing.
 */
import type { GitResponse, RunStatus, WorkerItem } from "./types";

export interface CurrentThread {
  /** The id in the route; `active` for legacy links. */
  routeId: string;
  /** The resolved thread id. */
  id: string;
  title: string;
  run: RunStatus | null;
  workerCount: number;
  workers: WorkerItem[] | null;
  git: GitResponse | null;
  model: string | null;
  contextPct: number | null;
}

export const currentThread = $state<{ value: CurrentThread | null }>({ value: null });
