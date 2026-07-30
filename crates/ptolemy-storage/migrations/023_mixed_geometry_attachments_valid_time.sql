-- Three things a third-party source (KML and friends) carries that the schema
-- could not hold. datasets.geometry_type is free text with no CHECK, so the new
-- 'geometry' value needs no DDL here; only the Rust parse sites change.

-- ─── Attachments may belong to a dataset instead of a feature ────────
-- A style's icon or overlay image belongs to the dataset, not to any one
-- feature, so the two owner shapes are exclusive rather than optional extras.

ALTER TABLE attachments ALTER COLUMN feature_id DROP NOT NULL;
ALTER TABLE attachments ALTER COLUMN branch_id DROP NOT NULL;
ALTER TABLE attachments ADD COLUMN IF NOT EXISTS dataset_id UUID;

-- separate from ADD COLUMN so a re-run restores the FK even when the column
-- already exists
ALTER TABLE attachments DROP CONSTRAINT IF EXISTS attachments_dataset_id_fkey;
ALTER TABLE attachments ADD CONSTRAINT attachments_dataset_id_fkey
    FOREIGN KEY (dataset_id) REFERENCES datasets(id) ON DELETE CASCADE;

ALTER TABLE attachments DROP CONSTRAINT IF EXISTS attachments_one_owner;
ALTER TABLE attachments ADD CONSTRAINT attachments_one_owner CHECK (
    (feature_id IS NOT NULL AND branch_id IS NOT NULL AND dataset_id IS NULL)
    OR (dataset_id IS NOT NULL AND feature_id IS NULL AND branch_id IS NULL)
);

CREATE INDEX IF NOT EXISTS idx_attachments_dataset ON attachments(dataset_id);

-- ─── Feature versions carry a valid time ─────────────────────────────
-- Both NULL means no time was recorded, which is what every existing row and
-- every writer that does not set them gets. The range is half-open,
-- [valid_from, valid_to), so adjacent ranges do not both match an instant.

ALTER TABLE feature_versions ADD COLUMN IF NOT EXISTS valid_from TIMESTAMPTZ;
ALTER TABLE feature_versions ADD COLUMN IF NOT EXISTS valid_to TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_feature_versions_valid
    ON feature_versions(dataset_id, valid_from, valid_to);

-- The view gains two columns, which CREATE OR REPLACE cannot do. Body is
-- 020_features_view_chain.sql unchanged apart from those columns.
DROP VIEW IF EXISTS features;

CREATE VIEW features AS
WITH RECURSIVE branch_chains AS (
    SELECT b.id AS branch_id, c.id AS changeset_id, c.parent_id
    FROM branches b
    JOIN changesets c ON c.id = b.head
  UNION ALL
    SELECT bc.branch_id, c.id, c.parent_id
    FROM changesets c
    JOIN branch_chains bc ON bc.parent_id = c.id
),
latest_versions AS (
    SELECT DISTINCT ON (bc.branch_id, fv.feature_id)
        fv.feature_id AS id,
        bc.branch_id,
        fv.dataset_id,
        fv.operation,
        fv.geometry,
        fv.properties,
        fv.created_at,
        fv.valid_from,
        fv.valid_to
    FROM feature_versions fv
    JOIN branch_chains bc ON fv.changeset_id = bc.changeset_id
    ORDER BY bc.branch_id, fv.feature_id, fv.created_at DESC, fv.id DESC
)
SELECT id, branch_id, dataset_id, geometry, properties, created_at,
       valid_from, valid_to
FROM latest_versions
WHERE operation != 'delete';
