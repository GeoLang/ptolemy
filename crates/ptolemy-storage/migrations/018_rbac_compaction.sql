-- RBAC: per-dataset and per-branch permissions
CREATE TABLE IF NOT EXISTS dataset_permissions (
    id UUID PRIMARY KEY,
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    permission TEXT NOT NULL,  -- 'read', 'write', 'admin'
    granted_by TEXT NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_dataset_perm_unique
    ON dataset_permissions(dataset_id, user_id);

CREATE TABLE IF NOT EXISTS branch_permissions (
    id UUID PRIMARY KEY,
    branch_id UUID NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    permission TEXT NOT NULL,  -- 'read', 'write', 'admin'
    granted_by TEXT NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_branch_perm_unique
    ON branch_permissions(branch_id, user_id);

-- Version compaction tracking
CREATE TABLE IF NOT EXISTS compaction_runs (
    id UUID PRIMARY KEY,
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    branch_id UUID REFERENCES branches(id) ON DELETE SET NULL,
    versions_before BIGINT NOT NULL DEFAULT 0,
    versions_after BIGINT NOT NULL DEFAULT 0,
    versions_removed BIGINT NOT NULL DEFAULT 0,
    bytes_freed BIGINT NOT NULL DEFAULT 0,
    keep_latest INTEGER NOT NULL DEFAULT 1,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'running'  -- 'running', 'completed', 'failed'
);

CREATE INDEX IF NOT EXISTS idx_compaction_dataset ON compaction_runs(dataset_id, started_at DESC);
