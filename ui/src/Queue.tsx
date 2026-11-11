import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { fetchExits, fetchLanes } from "./api";
import type { ActiveStepView } from "./api/ActiveStepView";
import type { ChecksView } from "./api/ChecksView";
import type { EntryView } from "./api/EntryView";
import type { ExitView } from "./api/ExitView";
import type { LaneView } from "./api/LaneView";
import { reviewBadge, reviewLine } from "./review";
import { Avatar, primary, timeAgo } from "./ui";

/*
 * The queues, read only: each lane's PRs in the order they merge, and why each is held. A PR
 * opens on GitHub, where it is queued, reviewed and answered.
 */

const laneKey = (l: LaneView) => `${l.repo}:${l.trunk}`;

/** How far back the lane lists a PR the queue ejected, while it is not queued again. */
const EJECTED_DAYS = 7;

/** Two panes: the lanes, and the selected lane's PRs in merge order. A narrow screen stacks
 * them. */
export function Queue({ workspace }: { workspace: number }) {
  const { lane: laneId } = useSearch({ from: "/$account/queue" });
  const navigate = useNavigate({ from: "/$account/queue" });
  const { account } = useParams({ from: "/$account/queue" });
  const lanes = useQuery({
    queryKey: ["lanes", workspace],
    queryFn: () => fetchLanes(workspace),
    refetchInterval: 3000,
  });
  const current = lanes.data?.find((l) => laneKey(l) === laneId) ?? lanes.data?.[0];
  const exits = useQuery({
    queryKey: ["exits", workspace, current?.repo, current?.trunk],
    queryFn: async () => {
      const exits: ExitView[] = [];
      let before: number | undefined;
      do {
        const page = await fetchExits(workspace, {
          repo: current?.repo,
          days: EJECTED_DAYS,
          before,
        });
        exits.push(...page.exits);
        before = page.next ?? undefined;
      } while (before !== undefined);
      return exits;
    },
    enabled: current !== undefined,
    refetchInterval: 30_000,
  });

  if (lanes.isError) return <p className="p-6 text-bad">Could not load the queue.</p>;
  if (lanes.data?.length === 0)
    return (
      <div className="grid h-full place-items-center text-center">
        <div>
          <p className="font-medium">No queues yet</p>
          <p className="mt-1 max-w-xs text-muted">
            Set up a repo, then add the{" "}
            <code className="rounded bg-sunken px-1 text-xs">mergeloop</code> label to a PR on
            GitHub.
          </p>
          <Link to="/$account/settings" params={{ account }} className={`${primary} mt-4`}>
            Set up a repo
          </Link>
        </div>
      </div>
    );
  if (!lanes.data) return null;

  const lane = current ?? lanes.data[0];
  const queued = lane.entries;
  const ejected = lastEjected(exits.data ?? [], lane);
  // The queue lists the active PRs first, so the batch is the head of the lane.
  const batched = lane.batch === null ? [] : queued.filter((e) => e.state === "active");
  const rest = queued.slice(batched.length);
  const inLine = rest.filter((e) => e.state !== "held");
  const reviewing = rest.filter(heldByReview).sort((a, b) => Number(asks(b)) - Number(asks(a)));
  const held = rest.filter((e) => e.state === "held" && !heldByReview(e));
  const place = (e: EntryView) =>
    e.state === "held" ? null : queued.findIndex((q) => q.number === e.number) + 1;
  return (
    <div className="flex h-full flex-col lg:grid lg:grid-cols-[220px_minmax(360px,1fr)]">
      <div className="shrink-0 border-line p-2 max-lg:flex max-lg:gap-1 max-lg:overflow-x-auto max-lg:border-b lg:overflow-y-auto lg:border-r">
        <p className="px-2 py-2 text-xs font-medium text-muted max-lg:hidden">Queues</p>
        {lanes.data.map((l) => (
          <button
            key={laneKey(l)}
            type="button"
            onClick={() => navigate({ search: { lane: laneKey(l) } })}
            className={`flex w-full items-center justify-between gap-3 rounded-md px-2 py-1.5 text-left max-lg:w-auto max-lg:shrink-0 ${
              l === lane ? "bg-surface shadow-xs ring-1 ring-line" : "hover:bg-sunken"
            }`}
          >
            <span className="truncate" title={l.repo}>
              {l.repo.split("/")[1]}
              <span className="text-faint"> → {l.trunk}</span>
            </span>
            <span className="flex items-center gap-1.5 text-xs text-faint tabular-nums">
              {l.entries.some((e) => e.state === "held") && (
                <span className="size-1.5 rounded-full bg-warn" title="A PR is held" />
              )}
              {l.entries.length}
            </span>
          </button>
        ))}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto bg-surface">
        <div className="sticky top-0 border-b border-line bg-surface px-4 py-3">
          <p className="font-medium">{lane.repo}</p>
          <p className="text-xs text-muted">Into {lane.trunk}</p>
        </div>
        <ListHead title="Merge lane" note="one at a time" count={batched.length + inLine.length} />
        <ol>
          {lane.batch !== null && (
            <li className="border-b border-line-soft">
              <p className="flex items-center gap-2 bg-sunken px-4 py-2 text-xs text-muted">
                <span className="font-medium text-fg-2">Batch</span>
                testing {batched.length} PRs together in
                <a
                  href={`https://github.com/${lane.repo}/pull/${lane.batch}`}
                  target="_blank"
                  rel="noreferrer"
                  className="tabular-nums text-fg-2 underline decoration-line-strong underline-offset-2 hover:text-fg hover:decoration-fg"
                >
                  #{lane.batch}
                </a>
              </p>
              <ol className="shadow-[inset_2px_0_0_0_var(--color-blue-400)]">
                {batched.map((e) => (
                  <li key={e.number}>
                    <Row repo={lane.repo} entry={e} place={place(e)} />
                  </li>
                ))}
              </ol>
            </li>
          )}
          {inLine.map((e) => (
            <li key={e.number}>
              <Row repo={lane.repo} entry={e} place={place(e)} />
            </li>
          ))}
        </ol>
        {lane.batch === null && inLine.length === 0 && <Empty>No PRs in the lane</Empty>}
        <Rows
          title="Getting ready"
          note="each on its own, then the line"
          items={reviewing}
          empty="No PRs getting ready"
        >
          {(e) => <Row repo={lane.repo} entry={e} place={null} track />}
        </Rows>
        <Rows title="Held" note="until GitHub would merge it" items={held} empty="No PRs held">
          {(e) => <Row repo={lane.repo} entry={e} place={null} />}
        </Rows>
        <Rows
          title="Ejected"
          note="label it again on GitHub to retry"
          items={ejected}
          empty={
            exits.isError
              ? "Could not load the ejected PRs."
              : exits.data
                ? `No PRs ejected in the last ${EJECTED_DAYS} days`
                : "Loading…"
          }
        >
          {(x) => <EjectedRow exit={x} />}
        </Rows>
      </div>
    </div>
  );
}

/** The PRs whose last exit from this lane, of the window, ejected them, and that are not queued
 * again: newest first. */
function lastEjected(exits: ExitView[], lane: LaneView): ExitView[] {
  const queued = new Set(lane.entries.flatMap((e) => [e.number, ...e.layers]));
  const last = new Map<number, ExitView>();
  for (const x of exits.filter((x) => x.trunk === lane.trunk)) {
    const seen = last.get(x.number);
    if (!seen || seen.at < x.at) last.set(x.number, x);
  }
  return [...last.values()]
    .filter((x) => x.outcome === "ejected" && !queued.has(x.number))
    .sort((a, b) => b.at.localeCompare(a.at));
}

/** A held PR that is getting ready or waits on the agent's review or its threads, not on
 * GitHub's own rules. */
const heldByReview = (e: EntryView) => reviewBadge(e) !== null;
/** A PR the agent's review asks a human about: the one thing here that a person can move. */
const asks = (e: EntryView) => e.hold?.reason === "review_asks";

const stateBadge: Record<EntryView["state"], [string, string]> = {
  active: [
    "Starting",
    "bg-blue-50 text-blue-700 ring-blue-600/20 dark:bg-blue-400/10 dark:text-blue-300 dark:ring-blue-400/30",
  ],
  waiting: ["Waiting", "bg-sunken text-fg-2 ring-neutral-500/20"],
  held: [
    "Held",
    "bg-amber-50 text-amber-800 ring-amber-600/20 dark:bg-amber-400/10 dark:text-amber-300 dark:ring-amber-400/30",
  ],
};

const stepLabel: Record<ActiveStepView, string> = {
  rebasing: "Rebasing",
  checks: "Running checks",
  fixing: "Fixing checks",
  merging: "Merging",
};

function State({ entry }: { entry: EntryView }) {
  const [label, style] =
    reviewBadge(entry) ??
    (entry.state === "held" && entry.hold?.reason === "review_required"
      ? ["Needs a reviewer", stateBadge.held[1]]
      : entry.step
        ? [stepLabel[entry.step], stateBadge.active[1]]
        : stateBadge[entry.state]);
  return <Badge style={style}>{label}</Badge>;
}

function Badge({ style, children }: { style: string; children: ReactNode }) {
  return (
    <span
      className={`whitespace-nowrap rounded-md px-1.5 py-0.5 text-xs font-medium ring-1 ring-inset ${style}`}
    >
      {children}
    </span>
  );
}

const ejectedBadge =
  "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-400/10 dark:text-red-300 dark:ring-red-400/30";

/** A band over a list of PRs. */
function ListHead({ title, note, count }: { title: string; note: string; count: number }) {
  return (
    <p className="flex items-center justify-between border-b border-line bg-sunken px-4 py-2 text-xs text-muted">
      <span>
        <span className="font-medium text-fg-2">{title}</span> · {note}
      </span>
      <span className="tabular-nums">{count}</span>
    </p>
  );
}

/** What a list says with no PRs in it. */
function Empty({ children }: { children: ReactNode }) {
  return <p className="px-4 py-3 text-xs text-faint">{children}</p>;
}

/** A list of PRs below the lane, with its band. */
function Rows<T extends { number: number }>({
  title,
  note,
  items,
  empty,
  children,
}: {
  title: string;
  note: string;
  items: T[];
  empty: string;
  children: (item: T) => ReactNode;
}) {
  return (
    <div className="mt-6 border-t border-line">
      <ListHead title={title} note={note} count={items.length} />
      {items.length === 0 && <Empty>{empty}</Empty>}
      <ol>
        {items.map((e) => (
          <li key={e.number}>{children(e)}</li>
        ))}
      </ol>
    </div>
  );
}

/** A PR saved before the queue kept its details has no title until the next tick. */
const titleOf = (e: EntryView) => e.title || `Pull request #${e.layers.at(-1) ?? e.number}`;
const refsOf = (e: EntryView) =>
  e.layers.length ? `Stack ${e.layers.map((n) => `#${n}`).join(" ")}` : `#${e.number}`;
const pullUrl = (repo: string, n: number) => `https://github.com/${repo}/pull/${n}`;

/** A row's look: a link to the PR on GitHub. */
const rowClass = "flex gap-3 border-b border-line-soft px-4 py-3 hover:bg-page";

function Row({
  repo,
  entry: e,
  place,
  track,
}: {
  repo: string;
  entry: EntryView;
  /** `null` for a held PR: it has no place until it is released. */
  place: number | null;
  /** Shows where it is on its way to the trunk. */
  track?: boolean;
}) {
  const line = reviewLine(e);
  return (
    <a
      href={e.url || pullUrl(repo, e.number)}
      target="_blank"
      rel="noreferrer"
      className={rowClass}
    >
      <span className="w-4 pt-px text-right text-xs text-faint tabular-nums">{place}</span>
      <span className="min-w-0 flex-1">
        <span className="flex items-start justify-between gap-3">
          <span className="truncate font-medium">{titleOf(e)}</span>
          <State entry={e} />
        </span>
        <span className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
          <span className="tabular-nums">{refsOf(e)}</span>
          {e.author && (
            <span className="flex items-center gap-1">
              <Avatar src={e.author.avatar_url} alt={e.author.login} size="size-4" />
              {e.author.login}
            </span>
          )}
          {e.priority && <span className="font-medium text-hot">↑ Priority</span>}
          {e.fork && <span title="From a fork">Fork</span>}
          {e.fix_budget > 0 && e.fixes > 0 && (
            <span>
              {e.fixes} of {e.fix_budget} fixes used
            </span>
          )}
          <span className="ml-auto flex items-center gap-2">
            <Checks checks={e.checks} />
            <Diff entry={e} />
          </span>
        </span>
        {track && (
          <span className="mt-2 block">
            <Track entry={e} />
          </span>
        )}
        {line ? (
          <span className="mt-1 block text-xs">{line}</span>
        ) : (
          e.detail && <span className="mt-1 block text-warn">{e.detail}</span>
        )}
      </span>
    </a>
  );
}

function EjectedRow({ exit: x }: { exit: ExitView }) {
  return (
    <a
      href={x.url || pullUrl(x.repo, x.number)}
      target="_blank"
      rel="noreferrer"
      className={rowClass}
    >
      <span className="w-4" />
      <span className="min-w-0 flex-1">
        <span className="flex items-start justify-between gap-3">
          <span className="truncate font-medium">{x.title || `Pull request #${x.number}`}</span>
          <Badge style={ejectedBadge}>Ejected</Badge>
        </span>
        <span className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
          <span className="tabular-nums">#{x.number}</span>
          {x.author && (
            <span className="flex items-center gap-1">
              <Avatar src={x.author.avatar_url} alt={x.author.login} size="size-4" />
              {x.author.login}
            </span>
          )}
          <span className="ml-auto">{timeAgo(x.at)}</span>
        </span>
        <span className="mt-1 block text-warn">{x.detail.replace(/^Ejected: /, "")}</span>
      </span>
    </a>
  );
}

const stages: [NonNullable<EntryView["stage"]>, string][] = [
  ["rebase", "Rebase"],
  ["checks", "Checks"],
  ["review", "Review"],
  ["lane", "Merge lane"],
];

/** How a stage's bar shows: done, where the PR is, or yet to come. */
function stageColor(e: EntryView, i: number) {
  const at = stages.findIndex(([s]) => s === e.stage);
  if (i < at) return "bg-ok";
  if (i > at) return "bg-line-strong/50";
  if (e.state === "waiting") return "bg-ok";
  if (e.hold?.reason === "review_asks" || e.hold?.reason === "review_changes")
    return "bg-amber-500";
  return "bg-blue-500 motion-safe:animate-pulse";
}

/** Where a queued PR is on its way to the trunk. */
function Track({ entry: e }: { entry: EntryView }) {
  return (
    <span className="flex w-full gap-1">
      {stages.map(([stage, name], i) => (
        <span
          key={stage}
          title={name}
          className={`block h-1 min-w-0 flex-1 rounded-full ${stageColor(e, i)}`}
        />
      ))}
    </span>
  );
}

const checksStyle: Record<ChecksView, [string, string, string]> = {
  passed: ["✓", "text-ok", "Checks passed"],
  failed: ["✕", "text-bad", "Checks failed"],
  pending: ["●", "text-warn", "Checks running"],
};

function Checks({ checks }: { checks: ChecksView | null }) {
  if (!checks) return null;
  const [icon, color, text] = checksStyle[checks];
  return (
    <span className={color} title={text}>
      {icon}
    </span>
  );
}

function Diff({ entry: e }: { entry: EntryView }) {
  if (!e.url) return null;
  return (
    <span className="font-mono tabular-nums">
      <span className="text-ok">+{e.additions}</span>{" "}
      <span className="text-bad">−{e.deletions}</span>
    </span>
  );
}
