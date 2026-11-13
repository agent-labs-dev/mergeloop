-- What a review's head changes since it left its base, as an id a restack keeps: a stack layer
-- whose own changes are those of its approved head keeps that approval when the layers below move.
ALTER TABLE agent_reviews ADD COLUMN diff_id TEXT;
