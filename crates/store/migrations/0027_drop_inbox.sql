-- The inbox went with the You tab: GitHub is where a PR waits on someone now. Nothing reads or
-- writes these tables since the release before this one.
DROP TABLE inbox_snoozed, inbox_seen, inbox_teams, inbox_pulls, inbox_repos;
