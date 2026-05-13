-- Feature attachments: binary blobs linked to features
CREATE TABLE IF NOT EXISTS attachments (
    id UUID PRIMARY KEY,
    feature_id UUID NOT NULL,
    branch_id UUID NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes BIGINT NOT NULL DEFAULT 0,
    data BYTEA NOT NULL,
    thumbnail BYTEA,  -- optional smaller preview
    metadata JSONB NOT NULL DEFAULT '{}',
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_attachments_feature ON attachments(feature_id);
CREATE INDEX IF NOT EXISTS idx_attachments_branch ON attachments(branch_id);

-- Schema evolution: track schema changes per dataset
CREATE TABLE IF NOT EXISTS schema_migrations (
    id UUID PRIMARY KEY,
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    description TEXT NOT NULL,
    migration_type TEXT NOT NULL,  -- 'add_field', 'remove_field', 'rename_field', 'change_type'
    field_name TEXT,
    old_definition JSONB,
    new_definition JSONB,
    applied_by TEXT NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    rollback_sql TEXT  -- optional SQL to undo
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_schema_migrations_version
    ON schema_migrations(dataset_id, version);

-- Replication peers: track connected replicas
CREATE TABLE IF NOT EXISTS replication_peers (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    endpoint_url TEXT,
    last_sync_changeset UUID REFERENCES changesets(id),
    last_sync_at TIMESTAMPTZ,
    direction TEXT NOT NULL DEFAULT 'bidirectional',  -- 'push', 'pull', 'bidirectional'
    status TEXT NOT NULL DEFAULT 'active',  -- 'active', 'paused', 'error'
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Change feed: ordered log of changes for replication consumers
CREATE TABLE IF NOT EXISTS change_feed (
    sequence_id BIGSERIAL PRIMARY KEY,
    changeset_id UUID NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
    branch_id UUID NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    operation_type TEXT NOT NULL,  -- 'commit', 'merge', 'branch_create', 'branch_delete'
    payload JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_change_feed_branch ON change_feed(branch_id, sequence_id);
CREATE INDEX IF NOT EXISTS idx_change_feed_changeset ON change_feed(changeset_id);
