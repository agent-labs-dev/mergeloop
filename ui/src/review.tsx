import type { ReactNode } from "react";
import type { EntryView } from "./api/EntryView";
import type { KindView } from "./api/KindView";

/*
 * Where a queued PR is in the agent's review, as the queue's rows show it. The review itself,
 * and its answers, are on the PR on GitHub.
 */

/** The kinds of finding as people say them. */
export const kindNames: Record<KindView, string> = {
  security: "Security",
  migrations: "Migrations",
  dependencies: "Dependencies",
  public_api: "Public API",
  other: "Everything else",
};

export const reviewTones = {
  reviewing:
    "bg-violet-50 text-violet-700 ring-violet-600/20 dark:bg-violet-400/10 dark:text-violet-300 dark:ring-violet-400/30",
  asks: "bg-amber-50 text-amber-800 ring-amber-600/20 dark:bg-amber-400/10 dark:text-amber-300 dark:ring-amber-400/30",
  sentBack:
    "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-400/10 dark:text-red-300 dark:ring-red-400/30",
};

type Gated = Pick<EntryView, "state" | "hold" | "review">;

/** The badge of a PR the review holds, in place of "Held". */
export function reviewBadge(e: Gated): [string, string] | null {
  if (e.state !== "held") return null;
  switch (e.hold?.reason) {
    case "prep_rebasing":
      return ["Rebasing", reviewTones.reviewing];
    case "prep_checks":
      return ["Checks", reviewTones.reviewing];
    case "prep_fixing":
      return ["Fixing", reviewTones.reviewing];
    case "reviewing":
      return ["Reviewing", reviewTones.reviewing];
    case "review_asks":
      return ["Needs a human", reviewTones.asks];
    case "review_changes":
      return ["Sent back", reviewTones.sentBack];
    case "unresolved_threads":
      return ["Unresolved comments", reviewTones.sentBack];
    default:
      return null;
  }
}

const people = (logins: string[]) => logins.map((l) => `@${l}`).join(", ");

/** The line under a queue row: where the PR is in the review. `null` without one to show. */
export function reviewLine(e: Gated): ReactNode {
  const r = e.review;
  if (!r) return null;
  if (e.state === "held") {
    switch (e.hold?.reason) {
      case "reviewing":
        return <span className="text-muted">The agent is reading the change…</span>;
      case "review_asks":
        return (
          <span className="text-warn">
            {r.failed
              ? "The agent could not review it: a human reviews it instead"
              : `${r.asks} ${r.asks === 1 ? "question" : "questions"} for ${
                  r.asked.length ? people(r.asked) : "a reviewer"
                } · ${r.cleared} cleared by the agent`}
          </span>
        );
      case "review_changes":
        return (
          <span className="text-muted">
            {r.answered_by
              ? `@${r.answered_by} asked for changes`
              : `${r.sent_back} sent back to the author`}{" "}
            · goes on once each comment is resolved
          </span>
        );
      case "unresolved_threads":
        return <span className="text-muted">{e.hold?.message}</span>;
      default:
        return null;
    }
  }
  if (r.status !== "approved") return null;
  return (
    <span className="text-muted">
      {r.answered_by
        ? `Approved by @${r.answered_by} on the agent's ${r.asks === 1 ? "question" : "questions"}`
        : `Agent approved alone · ${r.cleared} cleared`}
    </span>
  );
}
