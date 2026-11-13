import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { type ReactNode, useEffect } from "react";
import { fetchBuildSettings, fetchBuilds } from "./api";
import type { BuildView } from "./api/BuildView";
import { Badge, duration, Empty, ListHead, primary, RepoIcon, tints } from "./ui";

/*
 * The build lane, read only: each labeled issue a developer's daemon builds, who holds it, and
 * how far it is. Issues and PRs open on GitHub.
 */

/** A claim unheard for this long may be taken over by another daemon (the server's
 * `STALE_SECS`). */
const STALE_SECS = 30 * 60;

/** A build's state in words a person reads at a glance, and its tint. */
function stateOf(b: BuildView): [string, string] {
  switch (b.status) {
    case "ready":
      return ["Ready", tints.grey];
    case "claimed":
      return b.stage === "reviewing" ? ["Reviewing", tints.blue] : ["Building", tints.blue];
    case "landing":
      return ["Landing", tints.blue];
    case "waiting":
      return ["Needs a human", tints.amber];
    case "stuck":
      return ["Stuck", tints.red];
    case "merged":
      return ["Merged", tints.green];
  }
}

/** The lists, in the order a person reads them, and the statuses each holds. */
const LISTS: { title: string; note: string; empty: string; holds: BuildView["status"][] }[] = [
  {
    title: "In progress",
    note: "a daemon holds it",
    empty: "No builds in progress",
    holds: ["claimed", "landing", "waiting"],
  },
  {
    title: "Stuck",
    note: "take the label off and on again to retry",
    empty: "No builds stuck",
    holds: ["stuck"],
  },
  {
    title: "Ready",
    note: "for the next daemon that may push",
    empty: "No issues waiting",
    holds: ["ready"],
  },
  { title: "Merged", note: "until its issue closes", empty: "No builds merged", holds: ["merged"] },
];

/** Two panes: the repos, and the builds of the one picked (all by default). A narrow screen
 * stacks them. */
export function Builds({ workspace }: { workspace: number }) {
  const { repo } = useSearch({ from: "/$account/builds" });
  const navigate = useNavigate({ from: "/$account/builds" });
  // Read as often as the builds: someone may turn the lane off or on meanwhile.
  const settings = useQuery({
    queryKey: ["build-settings", workspace],
    queryFn: () => fetchBuildSettings(workspace),
    refetchInterval: 5000,
  });
  const on = settings.data?.enabled === true;
  const builds = useQuery({
    queryKey: ["builds", workspace],
    queryFn: () => fetchBuilds(workspace),
    enabled: on,
    refetchInterval: 5000,
  });
  // The builds refused: the lane may have been turned off since the settings were read.
  const { refetch } = settings;
  const refused = builds.isError;
  useEffect(() => {
    if (refused) void refetch();
  }, [refused, refetch]);

  if (settings.data && !on) return <Start off />;
  if (settings.isError || (builds.isError && !settings.isFetching))
    return <p className="p-6 text-bad">Could not load the builds.</p>;
  if (!builds.data) return null;
  if (builds.data.length === 0) return <Start off={false} />;

  const repos = [...new Set(builds.data.map((b) => b.repo))].sort();
  const shown = builds.data.filter((b) => !repo || b.repo === repo);
  const pick = (repo?: string) => navigate({ search: { repo } });
  return (
    <div className="flex h-full flex-col lg:grid lg:grid-cols-[220px_minmax(360px,1fr)]">
      <div className="shrink-0 border-line p-2 max-lg:flex max-lg:gap-1 max-lg:overflow-x-auto max-lg:border-b lg:overflow-y-auto lg:border-r">
        <p className="px-2 py-2 text-xs font-medium text-muted max-lg:hidden">Repos</p>
        <RepoButton label="All repos" builds={builds.data} selected={!repo} onClick={pick} />
        {repos.map((r) => (
          <RepoButton
            key={r}
            repo={r}
            label={r.split("/")[1]}
            builds={builds.data.filter((b) => b.repo === r)}
            selected={r === repo}
            onClick={pick}
          />
        ))}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto bg-surface">
        <div className="sticky top-0 z-10 border-b border-line bg-surface px-4 py-3">
          <p className="font-medium">{repo ?? "All repos"}</p>
          <p className="text-xs text-muted">Labeled issues, built by your developers' daemons</p>
        </div>
        {LISTS.map((list, i) => {
          const items = shown.filter((b) => list.holds.includes(b.status));
          return (
            <div key={list.title} className={i === 0 ? "" : "mt-6 border-t border-line"}>
              <ListHead title={list.title} note={list.note} count={items.length} />
              {items.length === 0 && <Empty>{list.empty}</Empty>}
              <ol>
                {items.map((b) => (
                  <li key={b.id}>
                    <Row build={b} />
                  </li>
                ))}
              </ol>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function RepoButton({
  repo,
  label,
  builds,
  selected,
  onClick,
}: {
  repo?: string;
  label: string;
  builds: BuildView[];
  selected: boolean;
  onClick: (repo?: string) => void;
}) {
  return (
    <button
      type="button"
      onClick={() => onClick(repo)}
      className={`flex w-full items-center justify-between gap-3 rounded-md px-2 py-1.5 text-left max-lg:w-auto max-lg:shrink-0 ${
        selected ? "bg-surface shadow-xs ring-1 ring-line" : "hover:bg-sunken"
      }`}
    >
      <span className="flex min-w-0 items-center gap-2" title={repo}>
        {repo && <RepoIcon repo={repo} />}
        <span className="truncate">{label}</span>
      </span>
      <span className="flex items-center gap-1.5 text-xs text-faint tabular-nums">
        {builds.some((b) => b.status === "stuck") && (
          <span className="size-1.5 rounded-full bg-bad" title="A build is stuck" />
        )}
        {builds.length}
      </span>
    </button>
  );
}

const link =
  "tabular-nums text-fg-2 underline decoration-line-strong underline-offset-2 hover:text-fg hover:decoration-fg";

/** One build: its issue and state, then where it is, who holds it, and its PR. A stuck build's
 * reason opens on demand, in full. */
function Row({ build: b }: { build: BuildView }) {
  const [label, style] = stateOf(b);
  const issueUrl = `https://github.com/${b.repo}/issues/${b.issue}`;
  return (
    <div className="flex gap-3 border-b border-line-soft px-4 py-3">
      <span className="pt-px">
        <RepoIcon repo={b.repo} />
      </span>
      <span className="min-w-0 flex-1">
        <span className="flex items-start justify-between gap-3">
          <a
            href={issueUrl}
            target="_blank"
            rel="noreferrer"
            className="min-w-0 truncate font-medium hover:underline"
          >
            {b.title || `Issue #${b.issue}`}
          </a>
          <Badge style={style}>{label}</Badge>
        </span>
        <span className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
          <span>
            {b.repo.split("/")[1]}{" "}
            <a href={issueUrl} target="_blank" rel="noreferrer" className={link}>
              #{b.issue}
            </a>
          </span>
          {b.holder && b.status !== "ready" && <Holder build={b} />}
          {b.pr !== null && (
            <a
              href={`https://github.com/${b.repo}/pull/${b.pr}`}
              target="_blank"
              rel="noreferrer"
              className="ml-auto"
            >
              PR <span className={link}>#{b.pr}</span>
            </a>
          )}
        </span>
        {b.status === "stuck" && b.reason && <Reason reason={b.reason} />}
      </span>
    </div>
  );
}

const held = (b: BuildView) => ["claimed", "landing", "waiting"].includes(b.status);

/** Who holds it, and when their daemon was last heard from while it does. */
function Holder({ build: b }: { build: BuildView }) {
  if (!held(b)) return <span>last held by {b.holder}</span>;
  const heard =
    b.heard_secs === null ? (
      <span className="text-warn">given back, for the next daemon</span>
    ) : b.heard_secs >= STALE_SECS ? (
      <span className="text-warn">not heard from for {duration(b.heard_secs)}</span>
    ) : (
      <span>heard from {duration(b.heard_secs)} ago</span>
    );
  return (
    <>
      <span className="text-fg-2">{b.holder}</span>
      <span className="text-faint">·</span>
      {heard}
    </>
  );
}

function Reason({ reason }: { reason: string }) {
  const [first, ...rest] = reason.trim().split("\n");
  if (rest.length === 0 && first.length <= 140)
    return <span className="mt-1 block text-xs text-warn">{first}</span>;
  return (
    <details className="group mt-1 text-xs">
      <summary className="cursor-pointer list-none text-warn [&::-webkit-details-marker]:hidden">
        <span className="group-open:hidden">
          <span className="line-clamp-1 inline">{first}</span>{" "}
          <span className="whitespace-nowrap text-muted underline underline-offset-2">
            Show why
          </span>
        </span>
        <span className="hidden text-muted underline underline-offset-2 group-open:inline">
          Hide
        </span>
      </summary>
      <p className="mt-1 whitespace-pre-wrap break-words rounded-md bg-sunken p-2 font-mono text-[11px] leading-relaxed text-fg-2">
        {reason.trim()}
      </p>
    </details>
  );
}

/** How to start building: with the lane off, or on with nothing yet to build. */
function Start({ off }: { off: boolean }) {
  const { account } = useParams({ from: "/$account/builds" });
  const steps: [string, ReactNode][] = [
    ...(off
      ? ([["Turn the build lane on", "in Settings. It is off until you do."]] as [
          string,
          ReactNode,
        ][])
      : []),
    [
      "Label an issue",
      <>
        with your repo's label (<Code>mergeloop</Code> unless its settings say otherwise).
      </>,
    ],
    [
      "Sign in on your machine",
      <>
        with <Code>mergeloop login</Code>, once.
      </>,
    ],
    [
      "Run the daemon",
      <>
        with <Code>mergeloop daemon</Code>. It builds each issue into a PR, and lands it.
      </>,
    ],
  ];
  return (
    <div className="grid h-full place-items-center p-4">
      <div className="max-w-sm">
        <p className="text-center font-medium">
          {off ? "The build lane is off" : "Nothing to build yet"}
        </p>
        <p className="mt-1 text-center text-muted">
          Your developers' machines build labeled issues into PRs, and land them through the queue.
        </p>
        <ol className="mt-5 space-y-3">
          {steps.map(([title, text], i) => (
            <li key={title} className="flex gap-3 text-sm">
              <span className="grid size-5 shrink-0 place-items-center rounded-full bg-sunken text-xs font-medium text-fg-2 tabular-nums">
                {i + 1}
              </span>
              <span>
                <span className="font-medium">{title}</span>{" "}
                <span className="text-muted">{text}</span>
              </span>
            </li>
          ))}
        </ol>
        {off && (
          <div className="mt-5 text-center">
            <Link to="/$account/settings" params={{ account }} hash="builds" className={primary}>
              Turn it on
            </Link>
          </div>
        )}
      </div>
    </div>
  );
}

function Code({ children }: { children: ReactNode }) {
  return <code className="rounded bg-sunken px-1 text-xs text-fg-2">{children}</code>;
}
