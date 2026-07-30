-- The original's reference when no single EPSG code names it, as its WKT
-- definition, which is how a compound reference (NAD83 + NAVD88 height, say)
-- has to be said. When this is set the native_geometry value is stamped srid
-- 0. Exactly one of (a nonzero srid on native_geometry, this column) names an
-- original's reference, and both absent means no distinct original, as before.

ALTER TABLE feature_versions ADD COLUMN IF NOT EXISTS native_crs_wkt TEXT;
