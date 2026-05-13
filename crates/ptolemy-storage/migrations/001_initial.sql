-- Ptolemy versioned geodatabase schema
-- Requires PostGIS extension

CREATE EXTENSION IF NOT EXISTS postgis;

-- Datasets: top-level container for a feature class
CREATE TABLE datasets (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    srid INTEGER NOT NULL DEFAULT 4326,
    geometry_type TEXT NOT NULL DEFAULT 'point',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT NOT NULL
);

-- Branches: named mutable pointers into the changeset DAG
CREATE TABLE branches (
    id UUID PRIMARY KEY,
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    head UUID, -- references changesets(id), added after table exists
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT NOT NULL,
    UNIQUE (dataset_id, name)
);

-- Changesets: immutable commits forming a DAG
CREATE TABLE changesets (
    id UUID PRIMARY KEY,
    branch_id UUID NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    parent_id UUID REFERENCES changesets(id),
    message TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Add FK from branches.head -> changesets.id
ALTER TABLE branches
    ADD CONSTRAINT fk_branches_head FOREIGN KEY (head) REFERENCES changesets(id);

-- Feature versions: each row is a feature at a specific changeset
-- A feature's "current" state on a branch = latest version reachable from branch head
CREATE TABLE feature_versions (
    id BIGSERIAL PRIMARY KEY,
    feature_id UUID NOT NULL,
    dataset_id UUID NOT NULL REFERENCES datasets(id) ON DELETE CASCADE,
    changeset_id UUID NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
    operation TEXT NOT NULL CHECK (operation IN ('insert', 'update', 'delete')),
    geometry geometry, -- PostGIS geometry, NULL for deletes
    properties JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Index for finding feature history
CREATE INDEX idx_feature_versions_feature_id ON feature_versions(feature_id);
CREATE INDEX idx_feature_versions_changeset_id ON feature_versions(changeset_id);
CREATE INDEX idx_feature_versions_dataset_changeset ON feature_versions(dataset_id, changeset_id);

-- Spatial index on geometry
CREATE INDEX idx_feature_versions_geom ON feature_versions USING GIST(geometry);

-- Conflicts: unresolved merge conflicts
CREATE TABLE conflicts (
    id UUID PRIMARY KEY,
    merge_changeset_id UUID NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
    feature_id UUID NOT NULL,
    base_version_id BIGINT REFERENCES feature_versions(id),
    ours_version_id BIGINT REFERENCES feature_versions(id),
    theirs_version_id BIGINT REFERENCES feature_versions(id),
    resolved BOOLEAN NOT NULL DEFAULT FALSE,
    resolution TEXT CHECK (resolution IN ('ours', 'theirs', 'manual')),
    resolved_version_id BIGINT REFERENCES feature_versions(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_conflicts_merge_changeset ON conflicts(merge_changeset_id);
