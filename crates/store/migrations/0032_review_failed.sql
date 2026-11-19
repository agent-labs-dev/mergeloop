-- A review the agent could not make is `failed`, no longer `asks` with an error: it asks no
-- human anything, and its author may retry it.
UPDATE agent_reviews SET status = 'failed' WHERE status = 'asks' AND error IS NOT NULL;
