/*
 * PROTOTYPE, throw away: how a stack shows in the queue. Three variants of the lane pane, on mock
 * data shaped like gantry's 21-layer stack, switchable with `?variant=A|B|C` or ← →.
 * Run: `pnpm prototype:stack`. Each layer needs title, author, review, checks and diff from the
 * API, which `EntryView.layers` (numbers only) does not carry yet.
 */
import { type ReactNode, StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "../index.css";

type Status = "approved" | "carried" | "reviewing" | "asks" | "sent_back" | "waiting";
type Checks = "passed" | "failed" | "pending" | null;

type Layer = {
  number: number;
  title: string;
  branch: string;
  author: string;
  status: Status;
  checks: Checks;
  additions: number;
  deletions: number;
  /** What its review says, when there is something to say. */
  line?: string;
  labeled?: boolean;
};

type Pr = {
  number: number;
  title: string;
  author: string;
  badge: [string, string];
  checks: Checks;
  additions: number;
  deletions: number;
  line?: string;
};

type Entry = { kind: "pr"; pr: Pr } | { kind: "stack"; number: number; layers: Layer[] };

const L = (
  number: number,
  title: string,
  branch: string,
  status: Status,
  additions: number,
  deletions: number,
  extra: Partial<Layer> = {},
): Layer => ({
  number,
  title,
  branch,
  author: "jnsdls",
  status,
  checks: "passed",
  additions,
  deletions,
  ...extra,
});

/** gantry's stack, bottom to top, as it stood: #30 waits on a human, the layers above it wait. */
const gantry: Layer[] = [
  L(
    35,
    "ci: run on Blacksmith runners sized to each job",
    "ci/blacksmith-runners",
    "approved",
    84,
    31,
  ),
  L(
    18,
    "feat(manifest): parse, validate and plan the app manifest in Rust",
    "feat/rust-manifest",
    "approved",
    2140,
    12,
  ),
  L(
    19,
    "feat(stack): stack config with named targets and credential references",
    "feat/stack-config",
    "approved",
    612,
    40,
  ),
  L(
    20,
    "feat(credentials): resolve credential references and add gantry doctor",
    "feat/credentials",
    "carried",
    903,
    77,
  ),
  L(
    21,
    "feat(credentials): mint generated secrets once into the target's own store",
    "feat/mint",
    "approved",
    731,
    58,
  ),
  L(
    22,
    "feat(compose): gantry up for local and device targets",
    "feat/up-local",
    "carried",
    1288,
    140,
  ),
  L(
    23,
    "feat(manifest): derive target values with typed paths in literals",
    "feat/templates",
    "carried",
    455,
    61,
  ),
  L(
    24,
    "feat: add the Zero component, rendered as one zero-cache on Compose",
    "feat/zero",
    "carried",
    380,
    22,
  ),
  L(
    25,
    "feat(drift): declare drift checks in the manifest and evaluate them against git",
    "feat/drift",
    "carried",
    517,
    33,
  ),
  L(
    26,
    "feat: deploy a release read from the app's repository at its tag",
    "feat/release-source",
    "carried",
    664,
    90,
  ),
  L(
    27,
    "feat(cloud): render an aws target as chart values driven by the delivery plan",
    "feat/cloud-render",
    "approved",
    1102,
    214,
  ),
  L(
    28,
    "feat(terraform): one writer per aws secret entry, URLs composed in the cluster",
    "feat/aws-terraform",
    "approved",
    823,
    301,
  ),
  L(
    29,
    "feat(gantry): deploy an aws target with gantry up, Terraform then Helm",
    "feat/up-aws",
    "carried",
    978,
    120,
  ),
  L(
    30,
    "feat(doctor): check a target the way up does and report every problem with its fix",
    "feat/doctor",
    "asks",
    1410,
    233,
    {
      line: "1 question for @jnsdls · 6 cleared by the agent",
    },
  ),
  L(
    31,
    "feat: deploy and check a gcp target through the same core as aws",
    "feat/gcp",
    "waiting",
    1530,
    260,
  ),
  L(
    32,
    "feat(init): draft the manifest with a read-only agent and write only what validates",
    "feat/init",
    "waiting",
    890,
    75,
  ),
  L(
    33,
    "test: choose test host ports outside the ephemeral range",
    "fix/compose-port-flake",
    "waiting",
    41,
    18,
  ),
  L(
    36,
    "feat(doctor): propose manifest edits with --manifest, applied only on a yes",
    "feat/doctor-manifest",
    "waiting",
    602,
    44,
  ),
  L(37, "feat(cli): add gantry down for local and device targets", "feat/down", "waiting", 310, 12),
  L(
    38,
    "chore!: remove the Python package and the marketplace packaging",
    "chore/remove-python",
    "waiting",
    96,
    4812,
    {
      labeled: true,
      checks: "pending",
    },
  ),
];

/** A small green stack, in the merge lane. */
const small: Layer[] = [
  L(51, "refactor(api): split the handlers by resource", "refactor/handlers", "approved", 420, 380),
  L(52, "feat(api): paginate the events endpoint", "feat/paginate-events", "carried", 160, 12),
  L(
    53,
    "feat(ui): infinite scroll on the history page",
    "feat/history-scroll",
    "approved",
    210,
    40,
    {
      labeled: true,
    },
  ),
];

const tone = {
  blue: "bg-blue-50 text-blue-700 ring-blue-600/20 dark:bg-blue-400/10 dark:text-blue-300 dark:ring-blue-400/30",
  grey: "bg-sunken text-fg-2 ring-neutral-500/20",
  violet:
    "bg-violet-50 text-violet-700 ring-violet-600/20 dark:bg-violet-400/10 dark:text-violet-300 dark:ring-violet-400/30",
  amber:
    "bg-amber-50 text-amber-800 ring-amber-600/20 dark:bg-amber-400/10 dark:text-amber-300 dark:ring-amber-400/30",
  red: "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-400/10 dark:text-red-300 dark:ring-red-400/30",
  green:
    "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-400/10 dark:text-emerald-300 dark:ring-emerald-400/30",
};

const statusLook: Record<Status, { label: string; badge: string; dot: string; fill: string }> = {
  approved: { label: "Approved", badge: tone.green, dot: "bg-emerald-500", fill: "bg-emerald-500" },
  carried: {
    label: "Approval kept",
    badge: tone.green,
    dot: "bg-emerald-500 ring-2 ring-emerald-500/30",
    fill: "bg-emerald-500/70",
  },
  reviewing: {
    label: "Reviewing",
    badge: tone.violet,
    dot: "bg-violet-500",
    fill: "bg-violet-500 motion-safe:animate-pulse",
  },
  asks: { label: "Needs a human", badge: tone.amber, dot: "bg-amber-500", fill: "bg-amber-500" },
  sent_back: { label: "Sent back", badge: tone.red, dot: "bg-red-500", fill: "bg-red-500" },
  waiting: {
    label: "Not reviewed yet",
    badge: tone.grey,
    dot: "bg-line-strong",
    fill: "bg-line-strong/60",
  },
};

const lane: { merge: Entry[]; ready: Entry[] } = {
  merge: [
    {
      kind: "pr",
      pr: {
        number: 43,
        title: "ci: bump the python group across 1 directory with 3 updates",
        author: "dependabot",
        badge: ["Running checks", tone.blue],
        checks: "pending",
        additions: 12,
        deletions: 12,
      },
    },
    { kind: "stack", number: 50, layers: small },
    {
      kind: "pr",
      pr: {
        number: 15,
        title: "fix(preflight): block device deploys with invalid credentials",
        author: "jnsdls",
        badge: ["Waiting", tone.grey],
        checks: "passed",
        additions: 58,
        deletions: 9,
      },
    },
  ],
  ready: [{ kind: "stack", number: 34, layers: gantry }],
};

/* ---------- shared bits, copied from Queue.tsx's look ---------- */

function Badge({ style, children }: { style: string; children: ReactNode }) {
  return (
    <span
      className={`whitespace-nowrap rounded-md px-1.5 py-0.5 text-xs font-medium ring-1 ring-inset ${style}`}
    >
      {children}
    </span>
  );
}

function ChecksIcon({ checks }: { checks: Checks }) {
  if (!checks) return null;
  const [icon, color] =
    checks === "passed"
      ? ["✓", "text-ok"]
      : checks === "failed"
        ? ["✕", "text-bad"]
        : ["●", "text-warn"];
  return <span className={color}>{icon}</span>;
}

function Diff({ a, d }: { a: number; d: number }) {
  return (
    <span className="font-mono tabular-nums">
      <span className="text-ok">+{a}</span> <span className="text-bad">−{d}</span>
    </span>
  );
}

function Avatar({ login }: { login: string }) {
  return (
    <img
      src={`https://github.com/${login}.png?size=40`}
      alt={login}
      className="size-4 shrink-0 rounded-full bg-sunken ring-1 ring-line"
    />
  );
}

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

function PrRow({ pr, place }: { pr: Pr; place: number | null }) {
  return (
    <div className="flex gap-3 border-b border-line-soft px-4 py-3 hover:bg-page">
      <span className="w-4 pt-px text-right text-xs text-faint tabular-nums">{place}</span>
      <span className="min-w-0 flex-1">
        <span className="flex items-start justify-between gap-3">
          <span className="truncate font-medium">{pr.title}</span>
          <Badge style={pr.badge[1]}>{pr.badge[0]}</Badge>
        </span>
        <span className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
          <span className="tabular-nums">#{pr.number}</span>
          <span className="flex items-center gap-1">
            <Avatar login={pr.author} />
            {pr.author}
          </span>
          <span className="ml-auto flex items-center gap-2">
            <ChecksIcon checks={pr.checks} />
            <Diff a={pr.additions} d={pr.deletions} />
          </span>
        </span>
      </span>
    </div>
  );
}

/** The stack as a whole: what holds it, and its counts. */
function summary(layers: Layer[]) {
  const holder = layers.find((l) => l.status !== "approved" && l.status !== "carried");
  const done = layers.filter((l) => l.status === "approved" || l.status === "carried").length;
  const top = layers[layers.length - 1];
  const additions = layers.reduce((s, l) => s + l.additions, 0);
  const deletions = layers.reduce((s, l) => s + l.deletions, 0);
  const waiting = holder ? layers.filter((l) => l.status === "waiting").length : 0;
  return { holder, done, top, additions, deletions, waiting };
}

const stackBadge = (layers: Layer[], inLane: boolean): [string, string] => {
  const { holder } = summary(layers);
  if (!holder) return inLane ? ["Waiting", tone.grey] : ["Approved", tone.green];
  return [
    `${statusLook[holder.status].label} on #${holder.number}`,
    statusLook[holder.status].badge,
  ];
};

type StackProps = { number: number; layers: Layer[]; place: number | null; inLane: boolean };

/* ---------- A: nested rows ---------- */

/** The stack's row, with each layer as an indented row under it, top first, on a rail down to
 * the trunk. Runs of layers alike fold into one line. */
function StackA({ layers, place, inLane }: StackProps) {
  const s = summary(layers);
  const [badge, style] = stackBadge(layers, inLane);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const top = [...layers].reverse();
  // A long stack folds runs of 4+ alike layers that are not the holder; a short one shows all.
  const fold = layers.length > 8;
  const groups: (Layer | Layer[])[] = [];
  for (const l of top) {
    const last = groups[groups.length - 1];
    const same = (a: Layer) => (a.status === "carried" ? "approved" : a.status);
    if (Array.isArray(last) && same(last[0]) === same(l) && l !== s.holder) last.push(l);
    else if (l === s.holder) groups.push(l);
    else groups.push([l]);
  }
  return (
    <div className="border-b border-line-soft">
      <div className="flex gap-3 px-4 pt-3 pb-2">
        <span className="w-4 pt-px text-right text-xs text-faint tabular-nums">{place}</span>
        <span className="min-w-0 flex-1">
          <span className="flex items-start justify-between gap-3">
            <span className="flex min-w-0 items-center gap-2">
              <StackGlyph />
              <span className="truncate font-medium">Stack of {layers.length} PRs</span>
              <span className="truncate text-muted">· {s.top.branch}</span>
            </span>
            <Badge style={style}>{badge}</Badge>
          </span>
          <span className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
            <span>
              {s.done} of {layers.length} approved
            </span>
            {s.holder && s.waiting > 0 && (
              <span>
                · {s.waiting} wait for #{s.holder.number}
              </span>
            )}
            <span className="flex items-center gap-1">
              · <Avatar login={s.top.author} />
              {s.top.author}
            </span>
            <span className="ml-auto">
              <Diff a={s.additions} d={s.deletions} />
            </span>
          </span>
        </span>
      </div>
      <ol className="relative mr-4 mb-3 ml-11 border-l border-line-strong">
        {groups.map((g) => {
          if (!Array.isArray(g) || g.length < 4 || !fold) {
            return (Array.isArray(g) ? g : [g]).map((l) => (
              <LayerRowA key={l.number} layer={l} holder={l === s.holder} />
            ));
          }
          const key = `${g[0].number}`;
          if (open.has(key))
            return g.map((l) => <LayerRowA key={l.number} layer={l} holder={false} />);
          const look = statusLook[g[0].status];
          return (
            <li key={key}>
              <button
                type="button"
                onClick={() => setOpen(new Set([...open, key]))}
                className="relative flex w-full items-center gap-2 py-1.5 pl-4 text-left text-xs text-muted hover:text-fg"
              >
                <span className={`absolute -left-[4.5px] size-2 rounded-full ${look.dot}`} />
                <span className="tabular-nums">
                  Layers {layers.indexOf(g[g.length - 1]) + 1}–{layers.indexOf(g[0]) + 1}
                </span>
                <span>
                  · {g.length} PRs{" "}
                  {g[0].status === "waiting" && s.holder
                    ? `waiting for #${s.holder.number}`
                    : "approved"}
                </span>
                <span className="text-faint underline underline-offset-2">Show titles</span>
              </button>
            </li>
          );
        })}
        <li className="relative pl-4 pt-1 text-xs text-faint">
          <span className="absolute -left-[4.5px] top-2 size-2 rounded-full border border-line-strong bg-surface" />
          main
        </li>
      </ol>
    </div>
  );
}

function LayerRowA({ layer: l, holder }: { layer: Layer; holder: boolean }) {
  const look = statusLook[l.status];
  return (
    <li className={`relative py-1.5 pl-4 ${holder ? "my-1 rounded-r-md bg-amber-500/5" : ""}`}>
      <span className={`absolute top-3 -left-[4.5px] size-2 rounded-full ${look.dot}`} />
      <span className="flex items-center gap-2">
        <span className="w-8 shrink-0 text-xs text-faint tabular-nums">#{l.number}</span>
        <span className={`min-w-0 flex-1 truncate ${holder ? "font-medium" : "text-fg-2"}`}>
          {l.title}
        </span>
        {l.labeled && <span className="text-xs text-faint">labeled</span>}
        <Badge style={look.badge}>{look.label}</Badge>
        <span className="flex w-28 shrink-0 items-center justify-end gap-2 text-xs">
          <ChecksIcon checks={l.checks} />
          <Diff a={l.additions} d={l.deletions} />
        </span>
      </span>
      {l.line && <span className="mt-0.5 block pl-10 text-xs text-warn">{l.line}</span>}
    </li>
  );
}

function StackGlyph() {
  return (
    <svg
      viewBox="0 0 16 16"
      className="size-4 shrink-0 text-muted"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
      aria-hidden
    >
      <rect x="3" y="2" width="10" height="3" rx="1" />
      <rect x="3" y="6.5" width="10" height="3" rx="1" />
      <rect x="3" y="11" width="10" height="3" rx="1" />
    </svg>
  );
}

/* ---------- B: blocks and a detail pane ---------- */

/** The stack drawn as a pile of blocks, one per layer, coloured by its review: pick one to read
 * it beside the pile. The holder is picked at first. */
function StackB({ layers, place, inLane }: StackProps) {
  const s = summary(layers);
  const [badge, style] = stackBadge(layers, inLane);
  const [picked, setPicked] = useState<number>(s.holder?.number ?? s.top.number);
  const p = layers.find((l) => l.number === picked) ?? s.top;
  const look = statusLook[p.status];
  return (
    <div className="border-b border-line-soft px-4 py-3">
      <div className="flex gap-3">
        <span className="w-4 pt-px text-right text-xs text-faint tabular-nums">{place}</span>
        <span className="flex min-w-0 flex-1 items-start justify-between gap-3">
          <span className="min-w-0">
            <span className="block truncate font-medium">{s.top.title}</span>
            <span className="mt-0.5 block text-xs text-muted">
              Stack of {layers.length} · {s.done} approved
              {s.holder && ` · held on #${s.holder.number}`}
            </span>
          </span>
          <Badge style={style}>{badge}</Badge>
        </span>
      </div>
      <div className="mt-3 ml-7 grid gap-4 sm:grid-cols-[200px_minmax(0,1fr)]">
        <ol className="flex flex-col-reverse gap-[3px]">
          {layers.map((l, i) => {
            const lk = statusLook[l.status];
            const sel = l.number === picked;
            return (
              <li key={l.number}>
                <button
                  type="button"
                  onClick={() => setPicked(l.number)}
                  title={`#${l.number} ${l.title}`}
                  style={{
                    marginLeft: `${Math.min(i, 20) * 1.5}px`,
                    width: `calc(100% - ${Math.min(i, 20) * 1.5}px)`,
                  }}
                  className={`flex h-5 items-center gap-2 rounded-sm px-1.5 text-left text-[11px] ${lk.fill} ${
                    sel
                      ? "ring-2 ring-fg ring-offset-1 ring-offset-surface"
                      : "opacity-90 hover:opacity-100"
                  } ${l.status === "waiting" ? "text-fg-2" : "text-white"}`}
                >
                  <span className="tabular-nums">#{l.number}</span>
                  <span className="truncate opacity-80">{l.branch}</span>
                </button>
              </li>
            );
          })}
        </ol>
        <div className="self-start rounded-lg border border-line p-3">
          <p className="flex items-start justify-between gap-3">
            <span className="text-xs text-muted tabular-nums">
              Layer {layers.indexOf(p) + 1} of {layers.length} · #{p.number}
            </span>
            <Badge style={look.badge}>{look.label}</Badge>
          </p>
          <p className="mt-1 font-medium">{p.title}</p>
          <p className="mt-1 flex flex-wrap items-center gap-2 text-xs text-muted">
            <Avatar login={p.author} />
            {p.author}
            <span className="font-mono">{p.branch}</span>
            <span className="ml-auto flex items-center gap-2">
              <ChecksIcon checks={p.checks} />
              <Diff a={p.additions} d={p.deletions} />
            </span>
          </p>
          {p.line && <p className="mt-2 text-xs text-warn">{p.line}</p>}
          {p.status === "waiting" && s.holder && (
            <p className="mt-2 text-xs text-muted">
              Reviewed once #{s.holder.number} below it is through.
            </p>
          )}
          {p.status === "carried" && (
            <p className="mt-2 text-xs text-muted">
              A restack moved it; its own changes are the ones the agent approved.
            </p>
          )}
          <p className="mt-3 text-xs">
            <span className="underline decoration-line-strong underline-offset-2">
              Open on GitHub ↗
            </span>
          </p>
        </div>
      </div>
    </div>
  );
}

/* ---------- C: one row with a strip, a table on open ---------- */

/** Today's row, with a strip of one segment per layer, bottom to top. Opening it lists the
 * layers as a table. */
function StackC({ layers, place, inLane }: StackProps) {
  const s = summary(layers);
  const [badge, style] = stackBadge(layers, inLane);
  const [open, setOpen] = useState(s.holder !== undefined);
  return (
    <div className="border-b border-line-soft">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex w-full gap-3 px-4 py-3 text-left hover:bg-page"
      >
        <span className="w-4 pt-px text-right text-xs text-faint tabular-nums">{place}</span>
        <span className="min-w-0 flex-1">
          <span className="flex items-start justify-between gap-3">
            <span className="truncate font-medium">{s.top.title}</span>
            <Badge style={style}>{badge}</Badge>
          </span>
          <span className="mt-2 flex h-2 gap-[2px]">
            {layers.map((l) => (
              <span
                key={l.number}
                title={`#${l.number} · ${statusLook[l.status].label}`}
                className={`block min-w-0 flex-1 rounded-[2px] ${statusLook[l.status].fill} ${
                  l === s.holder ? "outline-2 outline-offset-1 outline-amber-500" : ""
                }`}
              />
            ))}
          </span>
          <span className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted">
            <span className="tabular-nums">
              Stack #{layers[0].number} … #{s.top.number} · {layers.length} PRs
            </span>
            <span className="flex items-center gap-1">
              <Avatar login={s.top.author} />
              {s.top.author}
            </span>
            <span className="ml-auto flex items-center gap-2">
              <span className="text-faint">{open ? "Hide layers ▴" : "Show layers ▾"}</span>
              <Diff a={s.additions} d={s.deletions} />
            </span>
          </span>
          {s.holder?.line && (
            <span className="mt-1 block text-xs text-warn">
              #{s.holder.number}: {s.holder.line}
            </span>
          )}
        </span>
      </button>
      {open && (
        <div className="mx-4 mb-3 ml-11 overflow-x-auto rounded-md border border-line">
          <table className="w-full table-fixed text-xs">
            <thead className="bg-sunken text-left text-muted">
              <tr>
                <th className="w-12 px-2 py-1.5 font-medium">Layer</th>
                <th className="w-12 px-2 py-1.5 font-medium">PR</th>
                <th className="px-2 py-1.5 font-medium">Title</th>
                <th className="w-36 px-2 py-1.5 font-medium">Review</th>
                <th className="w-16 px-2 py-1.5 text-right font-medium">Checks</th>
                <th className="w-28 px-2 py-1.5 text-right font-medium">Diff</th>
              </tr>
            </thead>
            <tbody>
              {[...layers].reverse().map((l) => (
                <tr
                  key={l.number}
                  className={`border-t border-line-soft ${l === s.holder ? "bg-amber-500/5" : ""} ${
                    l.status === "waiting" ? "text-muted" : ""
                  }`}
                >
                  <td className="px-2 py-1.5 text-faint tabular-nums">{layers.indexOf(l) + 1}</td>
                  <td className="px-2 py-1.5 tabular-nums">#{l.number}</td>
                  <td className="truncate px-2 py-1.5" title={l.title}>
                    {l.title}
                  </td>
                  <td className="px-2 py-1.5">
                    <span className="flex items-center gap-1.5 whitespace-nowrap">
                      <span className={`size-1.5 rounded-full ${statusLook[l.status].dot}`} />
                      {statusLook[l.status].label}
                    </span>
                  </td>
                  <td className="px-2 py-1.5 text-right">
                    <ChecksIcon checks={l.checks} />
                  </td>
                  <td className="px-2 py-1.5 text-right whitespace-nowrap">
                    <Diff a={l.additions} d={l.deletions} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/* ---------- the page, and the switcher ---------- */

const variants = {
  A: { name: "Nested rows on a rail", Stack: StackA },
  B: { name: "Block pile + detail", Stack: StackB },
  C: { name: "Strip + table", Stack: StackC },
} as const;
type Key = keyof typeof variants;
const keys = Object.keys(variants) as Key[];

function readVariant(): Key {
  const v = new URLSearchParams(location.search).get("variant");
  return keys.includes(v as Key) ? (v as Key) : "A";
}

function Page() {
  const [variant, setVariant] = useState<Key>(readVariant);
  const go = (step: number) => {
    const next = keys[(keys.indexOf(variant) + step + keys.length) % keys.length];
    const url = new URL(location.href);
    url.searchParams.set("variant", next);
    history.replaceState(null, "", url);
    setVariant(next);
  };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (t.closest("input, textarea, [contenteditable]")) return;
      if (e.key === "ArrowLeft") go(-1);
      if (e.key === "ArrowRight") go(1);
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  });
  const { Stack } = variants[variant];
  const render = (e: Entry, place: number | null, inLane: boolean) =>
    e.kind === "pr" ? (
      <PrRow key={e.pr.number} pr={e.pr} place={place} />
    ) : (
      <Stack
        key={`${variant}-${e.number}`}
        number={e.number}
        layers={e.layers}
        place={place}
        inLane={inLane}
      />
    );
  return (
    <div className="flex min-h-screen flex-col lg:grid lg:grid-cols-[220px_minmax(360px,1fr)]">
      <div className="border-line p-2 lg:border-r">
        <p className="px-2 py-2 text-xs font-medium text-muted">Queues</p>
        <div className="flex items-center justify-between rounded-md bg-surface px-2 py-1.5 shadow-xs ring-1 ring-line">
          <span>
            gantry<span className="text-faint"> → main</span>
          </span>
          <span className="flex items-center gap-1.5 text-xs text-faint tabular-nums">
            <span className="size-1.5 rounded-full bg-warn" />4
          </span>
        </div>
      </div>
      <div className="bg-surface pb-24">
        <div className="border-b border-line px-4 py-3">
          <p className="font-medium">agent-labs-dev/gantry</p>
          <p className="text-xs text-muted">Into main</p>
        </div>
        <ListHead title="Merge lane" note="one at a time" count={lane.merge.length} />
        {lane.merge.map((e, i) => render(e, i + 1, true))}
        <div className="mt-6 border-t border-line">
          <ListHead
            title="Getting ready"
            note="each on its own, then the line"
            count={lane.ready.length}
          />
          {lane.ready.map((e) => render(e, null, false))}
        </div>
      </div>
      <div className="fixed bottom-4 left-1/2 z-50 flex -translate-x-1/2 items-center gap-1 rounded-full bg-neutral-900 px-1.5 py-1.5 text-sm text-white shadow-lg ring-1 ring-white/10">
        <button
          type="button"
          onClick={() => go(-1)}
          className="rounded-full px-3 py-1 hover:bg-white/10"
        >
          ←
        </button>
        <span className="px-2 tabular-nums">
          <span className="font-semibold">{variant}</span>{" "}
          <span className="text-white/70">({variants[variant].name})</span>
        </span>
        <button
          type="button"
          onClick={() => go(1)}
          className="rounded-full px-3 py-1 hover:bg-white/10"
        >
          →
        </button>
      </div>
    </div>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("no #root");
createRoot(root).render(
  <StrictMode>
    <Page />
  </StrictMode>,
);
