# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

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
