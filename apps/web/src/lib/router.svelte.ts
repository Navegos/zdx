/**
 * Query-string router.
 *
 * Canonical shapes:
 *   ?view=threads                  (recent-thread browser)
 *   ?view=thread&id=<id>&tab=agent|transcript|artifacts|trajectory|changes
 *   ?view=thread&id=<id>&tab=artifacts&artifact=<key>   (one artifact opened)
 *   ?view=monitor&section=overview|agents|services|usage|config|workspaces|jobs
 *
 * The bot already publishes `?view=threads&id=…`, `?view=git&id=…` and bare
 * `startapp=<thread_id>` deep links, so those are accepted and normalized rather
 * than broken. `?view=threads` *with* an id stays a single-thread link for that
 * reason; only the bare form opens the list. A file's `Open artifact` link is
 * `startapp=a_<key>_<thread_id>` (see `artifactLink.ts`).
 */
import { isArtifactKey, parseStartParam } from "./artifactLink";
import { startParam } from "./telegram";

export type View = "thread" | "threads" | "monitor";
export type ThreadTab = "workers" | "agent" | "transcript" | "artifacts" | "trajectory" | "changes";
export type MonitorSection =
  | "overview"
  | "agents"
  | "services"
  | "usage"
  | "config"
  | "workspaces"
  | "jobs";

export interface Route {
  view: View;
  id: string;
  tab: ThreadTab;
  section: MonitorSection;
}

/** An artifact a link asked to open, until its thread's Artifacts tab takes it. */
interface LinkedArtifact {
  id: string;
  key: string;
}

const THREAD_TABS: ThreadTab[] = ["workers", "agent", "transcript", "artifacts", "trajectory", "changes"];
const MONITOR_SECTIONS: MonitorSection[] = [
  "overview",
  "agents",
  "services",
  "usage",
  "config",
  "workspaces",
  "jobs",
];

export const THREAD_TAB_LABELS: Record<ThreadTab, string> = {
  workers: "Workers",
  agent: "Agent",
  transcript: "Thread",
  artifacts: "Artifacts",
  trajectory: "Trajectory",
  changes: "Changes",
};

export const MONITOR_SECTION_LABELS: Record<MonitorSection, string> = {
  overview: "Overview",
  agents: "Active agents",
  services: "Services",
  usage: "Usage",
  config: "Config",
  workspaces: "Workspaces",
  jobs: "Jobs",
};

function readRoute(): { route: Route; linked: LinkedArtifact | null } {
  const { route, artifact } = readLocation();
  return { route, linked: artifact ? { id: route.id, key: artifact } : null };
}

function readLocation(): { route: Route; artifact: string } {
  const params = new URLSearchParams(window.location.search);
  const id = params.get("id")?.trim() || "";
  const rawView = params.get("view")?.trim() ?? "";
  const rawTab = params.get("tab")?.trim() as ThreadTab | undefined;
  const rawSection = params.get("section")?.trim() as MonitorSection | undefined;

  const tab = rawTab && THREAD_TABS.includes(rawTab) ? rawTab : "transcript";
  const section =
    rawSection && MONITOR_SECTIONS.includes(rawSection) ? rawSection : "overview";
  const rawArtifact = params.get("artifact")?.trim() ?? "";
  // Only on an explicit thread, never `active`: the preview can send feedback.
  const artifact =
    id &&
    id !== "active" &&
    (rawView === "thread" || rawView === "threads") &&
    tab === "artifacts" &&
    isArtifactKey(rawArtifact)
      ? rawArtifact
      : "";

  // Legacy link shapes published by the bot.
  if (rawView === "git") return { route: { view: "thread", id: id || "active", tab: "changes", section }, artifact: "" };
  if (rawView === "threads") {
    return { route: { view: "thread", id: id || "active", tab, section }, artifact };
  }

  if (rawView === "thread") return { route: { view: "thread", id: id || "active", tab, section }, artifact };
  if (rawView === "monitor") return { route: { view: "monitor", id: id || "active", tab, section }, artifact: "" };
  if (id) return { route: { view: "thread", id, tab, section }, artifact: "" };

  // Cold open from a Telegram deep link: startapp carries the thread id, or a
  // thread and the key of one of its artifacts. Telegram passes it both in
  // initData (`start_param`) and as the `tgWebAppStartParam` query parameter.
  const start = parseStartParam(startParam() || (params.get("tgWebAppStartParam") ?? ""));
  if (start) {
    const tab = start.artifact ? "artifacts" : "transcript";
    return { route: { view: "thread", id: start.id, tab, section }, artifact: start.artifact };
  }

  return { route: { view: "monitor", id: "active", tab, section }, artifact: "" };
}

class Router {
  current = $state<Route>({ view: "monitor", id: "active", tab: "transcript", section: "overview" });
  /** Deliberately not `$state`: taking it must not re-render the thread view
   *  (that would remount the pane that just opened the preview). */
  private linked: LinkedArtifact | null = null;

  constructor() {
    const read = readRoute();
    this.current = read.route;
    this.linked = read.linked;
    window.addEventListener("popstate", () => {
      const next = readRoute();
      this.linked = next.linked;
      this.current = next.route;
    });
    this.replace(this.current);
  }

  /**
   * The artifact key a link asked to open in `threadId`, once: the pending
   * link is dropped and removed from the URL, so a reload, Back, or a later
   * remount of the pane does not open it again.
   */
  takeLinkedArtifact(threadId: string): string {
    const linked = this.linked;
    if (!linked || linked.id !== threadId || this.current.view !== "thread" || this.current.id !== threadId) {
      return "";
    }
    this.linked = null;
    this.replace(this.current);
    return linked.key;
  }

  private url(route: Route): string {
    const params = new URLSearchParams({ view: route.view });
    if (route.view === "thread") {
      params.set("id", route.id);
      params.set("tab", route.tab);
      if (this.linked && this.linked.id === route.id && route.tab === "artifacts") {
        params.set("artifact", this.linked.key);
      }
    } else {
      params.set("section", route.section);
    }
    // Keep the dev fixture flag across navigation.
    if (new URLSearchParams(window.location.search).has("demo")) params.set("demo", "1");
    return `${window.location.pathname}?${params}`;
  }

  private push(route: Route): void {
    // Navigating anywhere abandons a link that was not taken yet.
    this.linked = null;
    this.current = route;
    // State must stay null: route is read back from the URL, and a $state proxy
    // is not structured-cloneable.
    window.history.pushState(null, "", this.url(route));
  }

  openThread(id: string, tab: ThreadTab = "transcript"): void {
    this.push({ ...this.current, view: "thread", id, tab });
  }

  openThreadList(): void {
    this.push({ ...this.current, view: "threads" });
  }

  setTab(tab: ThreadTab): void {
    this.push({ ...this.current, view: "thread", tab });
  }

  openMonitor(section: MonitorSection = "overview"): void {
    this.push({ ...this.current, view: "monitor", section });
  }

  replace(route: Route): void {
    window.history.replaceState(null, "", this.url(route));
  }
}

export const router = new Router();
