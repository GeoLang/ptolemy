-- External datasets: a dataset that is a read-only view over a PostGIS relation
-- ptolemy does not own. Reads substitute a derived table over that relation for
-- the "features" view; nothing is versioned and every write path rejects.
-- The three columns are meaningless apart, so the CHECK ties them together.

ALTER TABLE datasets
    ADD COLUMN external_table TEXT,
    ADD COLUMN external_id_column TEXT,
    ADD COLUMN external_geometry_column TEXT;

ALTER TABLE datasets ADD CONSTRAINT datasets_external_all_or_none CHECK (
    (external_table IS NULL
        AND external_id_column IS NULL
        AND external_geometry_column IS NULL)
    OR (external_table IS NOT NULL
        AND external_id_column IS NOT NULL
        AND external_geometry_column IS NOT NULL)
);
