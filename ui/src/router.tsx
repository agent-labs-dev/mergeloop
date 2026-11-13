import {
  createRootRoute,
  createRoute,
  createRouter,
  Navigate,
  redirect,
} from "@tanstack/react-router";
import { App } from "./App";
import type { OutcomeView } from "./api/OutcomeView";
import { Builds } from "./Builds";
import { History, LISTS, type List, OUTCOMES, WINDOWS, type Window } from "./History";
import { Permalink } from "./Permalink";
import { Queue } from "./Queue";
import { Settings } from "./Settings";
import { useWorkspace } from "./workspace";

/*
 * The dashboard's pages, one path each, so a reload lands where you were and PostHog sees each
 * move as a pageview. The setup and sign-in screens have no paths: the session picks them.
 */

const optional = <T,>(value: unknown, type: "string" | "number") =>
  typeof value === type ? (value as T) : undefined;

const oneOf = <T,>(options: readonly T[], value: unknown) =>
  options.includes(value as T) ? (value as T) : undefined;

const root = createRootRoute({
  component: App,
  // A path that names no page goes home, which picks a workspace.
  notFoundComponent: () => <Navigate to="/" replace />,
});

// The dashboard redirects from here to the workspace it remembers.
const home = createRoute({ getParentRoute: () => root, path: "/" });

/** A workspace, by its GitHub account. */
const workspace = createRoute({ getParentRoute: () => root, path: "$account" });

const workspaceHome = createRoute({
  getParentRoute: () => workspace,
  path: "/",
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/$account/queue", params, replace: true });
  },
});

/** The You tab's links, from before GitHub took its place: to the queue. */
const you = createRoute({
  getParentRoute: () => workspace,
  path: "you",
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/$account/queue", params, replace: true });
  },
});

const queue = createRoute({
  getParentRoute: () => workspace,
  path: "queue",
  /** The lane in view, as `owner/repo:trunk`: the first by default. */
  validateSearch: (search): { lane?: string } => ({ lane: optional(search.lane, "string") }),
  component: () => <Queue workspace={useWorkspace().workspace} />,
});

const builds = createRoute({
  getParentRoute: () => workspace,
  path: "builds",
  /** The repo in view: every repo by default. */
  validateSearch: (search): { repo?: string } => ({ repo: optional(search.repo, "string") }),
  component: () => <Builds workspace={useWorkspace().workspace} />,
});

const history = createRoute({
  getParentRoute: () => workspace,
  path: "history",
  /** What the side lists, the repo and the developer in view, the window in days, and the
   * outcome listed. Repos, every repo and developer, 30 days, and every outcome by default.
   * `pr` lists only that PR of `repo`, from any time. */
  validateSearch: (
    search,
  ): {
    list?: List;
    repo?: string;
    author?: string;
    days?: Window;
    outcome?: OutcomeView;
    pr?: number;
  } => ({
    list: oneOf(LISTS, search.list),
    repo: optional(search.repo, "string"),
    author: optional(search.author, "string"),
    days: oneOf(WINDOWS, search.days),
    outcome: oneOf(OUTCOMES, search.outcome),
    pr: optional(search.pr, "number"),
  }),
  component: () => <History workspace={useWorkspace().workspace} />,
});

/** The repo whose settings are open, for a tab that used to have them. */
const repos = createRoute({
  getParentRoute: () => workspace,
  path: "repos",
  validateSearch: (search): { repo?: string } => ({ repo: optional(search.repo, "string") }),
  beforeLoad: ({ params, search }) => {
    throw redirect({ to: "/$account/settings", params, search, replace: true });
  },
});

/** A PR's permalink, which the API hands out: it opens wherever the PR is. */
const pull = createRoute({
  getParentRoute: () => workspace,
  path: "$repo/pull/$number",
  component: Permalink,
});

const settings = createRoute({
  getParentRoute: () => workspace,
  path: "settings",
  /** The repo whose settings are open: the workspace's by default. */
  validateSearch: (search): { repo?: string } => ({ repo: optional(search.repo, "string") }),
  // A key per workspace: drafts belong to one, and switching keeps this route mounted.
  component: () => {
    const context = useWorkspace();
    return <Settings key={context.workspace} {...context} />;
  },
});

export const router = createRouter({
  routeTree: root.addChildren([
    home,
    workspace.addChildren([workspaceHome, you, queue, builds, history, repos, settings, pull]),
  ]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
