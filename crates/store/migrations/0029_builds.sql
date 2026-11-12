-- The build lane: each workspace's switch, its tasks, and when each repo's labeled issues were
-- last read from GitHub.
CREATE TABLE build_settings (
    installation_id BIGINT PRIMARY KEY,
    settings JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One per issue of a workspace with the lane on that was ever a task (open, with the repo's
-- label): a build outlives its issue's leaving the label, so that a holder keeps its claim and
-- an older read of the issues cannot bring back what a newer one ended.
CREATE TABLE builds (
    id BIGSERIAL PRIMARY KEY,
    installation_id BIGINT NOT NULL,
    repo TEXT NOT NULL,
    issue BIGINT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    -- The issue's assignees on GitHub, by login: its pre-assignment.
    assignees TEXT[] NOT NULL DEFAULT '{}',
    -- Whether the issue is a task now: open, with the repo's label. Only a task is claimed.
    task BOOLEAN NOT NULL DEFAULT true,
    -- When the row last stopped or started being a task: a row that is no task for a day is
    -- pruned.
    task_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The intake write (a webhook's, or a sync's reconciliation) that last wrote what the row
    -- knows of its issue, from `build_generations`: a sync that noted an older one leaves the
    -- row be.
    generation BIGINT NOT NULL DEFAULT 0,
    -- `ready`; held by a daemon: `claimed`, `landing` or `waiting`; `stuck` or `merged`.
    status TEXT NOT NULL DEFAULT 'ready',
    -- The login that holds the claim, or held it last.
    holder TEXT,
    -- Which of `holder`'s daemons holds it, as the daemon named itself when it claimed: another
    -- daemon of the same login is another holder.
    daemon TEXT,
    -- The claim assigned `holder` on the issue: giving it back takes that away again.
    mirrored BOOLEAN NOT NULL DEFAULT false,
    -- Which taking of the build its holder has: a call with an older one is refused.
    claim BIGINT NOT NULL DEFAULT 0,
    -- What the holder said it is doing: `building` or `reviewing`.
    stage TEXT,
    heartbeat_at TIMESTAMPTZ,
    pr BIGINT,
    -- Why a stuck build is stuck, and since when.
    reason TEXT,
    stuck_at TIMESTAMPTZ,
    -- When a sync last read a stuck build's issue events, to see whether its label came back.
    stuck_checked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (installation_id, repo, issue)
);

-- Orders the intake writes of each repo, which run one at a time under the repo's lock.
CREATE SEQUENCE build_generations;

CREATE TABLE build_syncs (
    installation_id BIGINT NOT NULL,
    repo TEXT NOT NULL,
    synced_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (installation_id, repo)
);
