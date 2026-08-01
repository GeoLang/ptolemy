-- ─── Attachments are soft deleted ────────────────────────────────────
-- The table kept no history, so a deleted attachment left nothing to diff and
-- the ArcGIS facade's change files could never report one. A delete now sets
-- deleted_at and the row stays, so a window over the column answers what went.
--
-- NULL means live. Every reader filters on that, and only the change-file query
-- looks at a tombstone.

ALTER TABLE attachments ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;

-- the change-file window is a time range over one branch's attachments, both
-- for what arrived and for what went
CREATE INDEX IF NOT EXISTS idx_attachments_branch_window
    ON attachments(branch_id, created_at, deleted_at);
