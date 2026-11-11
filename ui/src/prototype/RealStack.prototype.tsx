/* PROTOTYPE, throw away: Queue's real Row, on mock stacks. */
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import type { EntryView } from "../api/EntryView";
import type { LayerView } from "../api/LayerView";
import { Row } from "../Queue";
import "../index.css";

const jn = { login: "octocat", avatar_url: "https://github.com/octocat.png?size=40" };
const L = (number: number, title: string, head_ref: string, a: number, d: number): LayerView => ({
  number, title, head_ref, url: "", author: jn, additions: a, deletions: d, checks: "passed",
});
const gantry: LayerView[] = [
  L(35, "ci: run on Blacksmith runners sized to each job", "ci/blacksmith-runners", 84, 31),
  ...[18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29].map((n) => L(n, `feat: layer ${n} of the Rust port`, `feat/l${n}`, 500, 40)),
  L(30, "feat(doctor): check a target the way up does and report every problem with its fix", "feat/doctor", 1410, 233),
  ...[31, 32, 33, 36, 37].map((n) => L(n, `feat: layer ${n} after doctor`, `feat/l${n}`, 300, 20)),
  L(38, "chore!: remove the Python package and the marketplace packaging", "chore/remove-python", 96, 4812),
];
const base: EntryView = {
  number: 0, priority: false, state: "waiting", stage: "lane", step: null, fixes: 0, fix_budget: 3,
  detail: null, hold: null, layers: [], stack: [], review: null, title: "", url: "", author: jn,
  created_at: "", head_ref: "", fork: false, additions: 0, deletions: 0, changed_files: 0,
  review_decision: null, reviews: [], requested: [], checks: "passed",
};
const held: EntryView = {
  ...base, number: 34, state: "held", stage: "review", layers: gantry.map((l) => l.number), stack: gantry,
  hold: { reason: "review_asks", layer: 30, message: "" },
  review: { status: "asks", asks: 1, sent_back: 0, cleared: 6, asked: ["octocat"], answered_by: null, failed: false },
};
const small = [L(51, "refactor(api): split the handlers by resource", "refactor/handlers", 420, 380), L(52, "feat(api): paginate the events endpoint", "feat/paginate-events", 160, 12), L(53, "feat(ui): infinite scroll on the history page", "feat/history-scroll", 210, 40)];
const green: EntryView = { ...base, number: 50, layers: [51, 52, 53], stack: small };
const pr: EntryView = { ...base, number: 15, title: "fix(preflight): block device deploys with invalid credentials", url: "x", additions: 58, deletions: 9 };

const root = document.getElementById("root");
if (!root) throw new Error("no #root");
createRoot(root).render(
  <StrictMode>
    <div className="mx-auto max-w-4xl bg-surface">
      <Row repo="o/gantry" trunk="main" entry={green} place={1} />
      <Row repo="o/gantry" trunk="main" entry={pr} place={2} />
      <Row repo="o/gantry" trunk="main" entry={held} place={null} track />
    </div>
  </StrictMode>,
);
