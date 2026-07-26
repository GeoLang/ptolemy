-- Per-dataset visibility. 'public' keeps anonymous reads, 'private' requires the
-- caller to hold a permission row on the dataset or one of its branches.
-- Default 'public' so existing datasets keep serving the same reads.

ALTER TABLE datasets ADD COLUMN visibility TEXT NOT NULL DEFAULT 'public';

ALTER TABLE datasets ADD CONSTRAINT datasets_visibility_check
    CHECK (visibility IN ('public', 'private'));
