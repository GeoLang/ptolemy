-- Fix the "features" view: resolve each branch by walking its ancestor chain
-- from the branch head, so forked branches see features inherited from before
-- the fork point. Previously the view only included changesets created on the
-- branch itself (changesets.branch_id), which made forks appear empty.
-- Latest-version pick tiebreaks on fv.id (commit order), matching the Rust queries.

CREATE OR REPLACE VIEW features AS
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
        fv.created_at
    FROM feature_versions fv
    JOIN branch_chains bc ON fv.changeset_id = bc.changeset_id
    ORDER BY bc.branch_id, fv.feature_id, fv.created_at DESC, fv.id DESC
)
SELECT id, branch_id, dataset_id, geometry, properties, created_at
FROM latest_versions
WHERE operation != 'delete';
