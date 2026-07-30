# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- `geometry_type: "geometry"` on a dataset, for a source container whose features
  differ from each other. Distinct from `geometry_collection`, which is one
  feature whose geometry is a collection.

- Attachments may belong to a dataset instead of a feature, via
  `GET`/`POST /datasets/{id}/attachments`. A style's icon or overlay image
  belongs to no single feature. Download, meta and delete serve both kinds.

- A feature version carries an optional valid time, `valid_from` and `valid_to`,
  set per operation on commit and returned on reads. Both null means no time was
  recorded. `GET /branches/{id}/features?valid_at=<RFC3339>` keeps only the
  features whose half-open range `[valid_from, valid_to)` covers the instant.

- A feature version may carry the geometry as its source recorded it, before
  reprojection to 4326: `native_geometry_wkb_hex` on a commit operation, with
  its reference as exactly one of `native_srid` (an EPSG code) or
  `native_crs_wkt` (the WKT definition, for a reference no single code names,
  such as a compound one). Read back exactly with
  `GET /branches/{id}/features/{feature_id}/native`. NULL means the version has
  no distinct original: a 4326 source, an edit, or a repaired geometry. An
  original with srid 4326 or a blank WKT is stored as NULL rather than as a
  duplicate or an unstatable claim, an update never inherits the previous
  version's original, and a merge carries originals across unchanged.

### Fixed

- `POST /branches/{id}/import/geojson` and `/import/csv` imported nothing: every
  row failed on the `feature_versions` primary key and the missing `dataset_id`,
  and the endpoint still answered 200 with `imported: 0`. Both now build the
  parsed features into one changeset through the normal commit path, so imported
  data is visible to branch reads, and a request whose rows all fail answers 422.

- A write of 1000 rows or more refreshes planner statistics on `feature_versions`,
  `changesets` and `branches` after it commits, so reads served right after a bulk
  import are no longer planned against pre-import statistics. Tunable with
  `PTOLEMY_ANALYZE_ROW_THRESHOLD`.

## [0.1.0] - 2026-05-30

### Added

- Initial release.
