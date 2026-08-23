-- ─── A project's shared state ────────────────────────────────────────
-- One row per (project, key). The value is whatever the client puts there:
-- ViewTopia writes its map snapshot under 'map' and its dashboards under
-- 'dashboards'. The server neither reads nor validates the shape, so a viewer
-- change needs no migration here.
--
-- ON DELETE CASCADE: the state describes the project and is worth nothing
-- without it.

CREATE TABLE IF NOT EXISTS project_state (
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by TEXT NOT NULL,
    PRIMARY KEY (project_id, key)
);

-- ─── A project is a third attachment owner ───────────────────────────
-- The overlay bitmaps a project's map names belong to the project, not to any
-- dataset or feature, so they join the same exclusive owner set the CHECK
-- already held to.

ALTER TABLE attachments ADD COLUMN IF NOT EXISTS project_id UUID;

-- separate from ADD COLUMN so a re-run restores the FK even when the column
-- already exists
ALTER TABLE attachments DROP CONSTRAINT IF EXISTS attachments_project_id_fkey;
ALTER TABLE attachments ADD CONSTRAINT attachments_project_id_fkey
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE;

ALTER TABLE attachments DROP CONSTRAINT IF EXISTS attachments_one_owner;
ALTER TABLE attachments ADD CONSTRAINT attachments_one_owner CHECK (
    (feature_id IS NOT NULL AND branch_id IS NOT NULL AND dataset_id IS NULL AND project_id IS NULL)
    OR (dataset_id IS NOT NULL AND feature_id IS NULL AND branch_id IS NULL AND project_id IS NULL)
    OR (project_id IS NOT NULL AND feature_id IS NULL AND branch_id IS NULL AND dataset_id IS NULL)
);

CREATE INDEX IF NOT EXISTS idx_attachments_project ON attachments(project_id);
