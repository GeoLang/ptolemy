-- The geometry as the source recorded it, before reprojection to 4326. NULL
-- means this version has no distinct original: the source was already 4326, or
-- the version came from an editor working on the 4326 map. The column is bare
-- untyped geometry, so each value carries its own srid. It is read back by
-- feature id, never queried spatially, so it gets no index.

ALTER TABLE feature_versions ADD COLUMN IF NOT EXISTS native_geometry geometry;
