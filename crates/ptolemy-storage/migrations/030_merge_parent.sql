-- Second parent of a merge changeset: the source branch head it brought in.
-- NULL on an ordinary commit. Without it a merge base can never advance past
-- the fork point, so re-merging a branch redoes the whole merge every time.
ALTER TABLE changesets ADD COLUMN merge_parent_id UUID REFERENCES changesets(id);

CREATE INDEX IF NOT EXISTS idx_changesets_merge_parent ON changesets(merge_parent_id);
