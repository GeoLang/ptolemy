# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- 2026-07-31: `POST /arcgis/rest/services/{service}/FeatureServer/0/applyEdits`,
  so an Esri client edits ptolemy rather than only reading it. Form-encoded
  `adds`, `updates` and `deletes`, esriJSON geometry in EPSG:4326 or Web Mercator
  (3857/102100, converted in-process), and every edit in one request becomes one
  commit on the dataset's `main` branch through the same store path `/api/v1`
  commits take. Deliberate divergence from Esri: because the batch is one commit,
  any failure refuses the whole batch as an `error` object naming the cause
  instead of reporting per-row results. Object ids on an add are assigned by the
  service as max + 1 and a client-supplied one is ignored. An update merges the
  attributes it carries over the ones the feature holds and keeps its geometry
  when it sends none. A layer whose ids are row numbers rather than a real
  `objectid` column refuses every edit, because a delete renumbers the rest. An
  editable layer's metadata now says `Query,Create,Update,Delete` with
  `allowGeometryUpdates` and per-field `editable` true, and a row-number layer
  still says `Query`. Writes need a token with write access and the same
  per-dataset ladder as `/api/v1`, and reads stay anonymous. Esri clients have no
  header for a credential, so a `token` request parameter is accepted on
  `/arcgis` paths only, from the query string and not from a form body: putting a
  token in a URL means anything that records URLs records it, so prefer
  `Authorization: Bearer`, which still wins when both are sent. Auth refusals on
  the facade are Geoservices-shaped, HTTP 200 with error code 499 (token
  required), 498 (invalid token) or 403 (role or grant insufficient).

- 2026-07-31: A read-only ArcGIS FeatureServer (Geoservices REST) frontend at
  `/arcgis/rest/services`, so an Esri client connects to ptolemy unchanged. One
  dataset is one single-layer service, layer id 0, read from the dataset's `main`
  branch. Serves the catalog, the service root, the layer definition and
  `/query` (GET and form POST) with `objectIds`, `outFields`, `returnGeometry`,
  `returnCountOnly`, `returnIdsOnly`, `resultOffset`/`resultRecordCount` paging
  with `exceededTransferLimit`, an `esriGeometryEnvelope` intersects filter, and
  `f=json`, `f=pjson` or `f=geojson`. OBJECTID comes from an integer `objectid`
  field when the dataset's schema declares one and is otherwise synthesized from
  feature order. A parameter it cannot honor is refused rather than ignored, and
  refusals follow the Geoservices convention of HTTP 200 with an `error` object.
  Datasets whose `geometry_type` is `geometry` or `geometry_collection` are
  excluded: an Esri layer declares exactly one geometry type. Dataset visibility
  is enforced exactly as on the other read routes. Verified against verne's
  extractor: `verne inspect` and `verne extract` both run clean.

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
